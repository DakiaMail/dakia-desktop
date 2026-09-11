import { MantineProvider } from "@mantine/core";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import type { Provider } from "../types";
import { AccountSetup } from "./AccountSetup";

const mocks = vi.hoisted(() => ({
  openExternal: vi.fn(),
}));

vi.mock("../api", () => ({
  api: { openExternal: mocks.openExternal },
}));

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
};

async function settleComboboxUpdates() {
  await act(async () => {
    await new Promise<void>((resolve) => window.setTimeout(resolve, 0));
  });
}

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  vi.clearAllMocks();
});

describe("AccountSetup Gmail app passwords", () => {
  it("guides Gmail accounts to Google app passwords and never renders OAuth controls", async () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
        />
      </MantineProvider>,
    );

    fireEvent.change(screen.getByRole("textbox", { name: "Email address" }), {
      target: { value: "person@gmail.com" },
    });

    expect(
      screen.getByText("Enable Google 2-Step Verification."),
    ).toBeVisible();
    fireEvent.click(
      screen.getByRole("button", { name: "official app-password guide" }),
    );
    expect(mocks.openExternal).toHaveBeenCalledWith(
      "https://support.google.com/accounts/answer/185833?hl=en",
    );
    expect(
      screen.getByText("Paste that app password into Dakia."),
    ).toBeVisible();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Do not use your personal Gmail or regular Google Account password in Dakia.",
    );
    expect(
      screen.getByText(
        "Managed Google Workspace accounts or Advanced Protection may prevent app-password creation. In that case, the account cannot currently connect to Dakia.",
      ),
    ).toBeVisible();
    expect(screen.getByLabelText("Google app password")).toBeVisible();
    expect(
      screen.queryByRole("button", { name: /Continue with Gmail/i }),
    ).not.toBeInTheDocument();

    await settleComboboxUpdates();
  });

  it("requires and submits a Google app password through the normal account command", async () => {
    const onSave = vi.fn();
    render(
      <MantineProvider>
        <AccountSetup providers={[gmail]} saving={false} onSave={onSave} />
      </MantineProvider>,
    );

    fireEvent.change(screen.getByRole("textbox", { name: "Email address" }), {
      target: { value: "person@gmail.com" },
    });
    fireEvent.change(screen.getByRole("textbox", { name: "Your name" }), {
      target: { value: "Person" },
    });
    const submit = screen.getByRole("button", { name: "Add account" });
    expect(submit).toBeDisabled();

    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "   " },
    });
    expect(submit).toBeDisabled();

    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "abcd efgh ijkl mnop" },
    });
    expect(submit).toBeEnabled();
    fireEvent.click(submit);

    await waitFor(() =>
      expect(onSave).toHaveBeenCalledWith(
        expect.objectContaining({
          email: "person@gmail.com",
          display_name: "Person",
          provider_id: "gmail",
        }),
        "abcd efgh ijkl mnop",
      ),
    );
  });

  it("shows Gmail guidance when Gmail is manually selected", async () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
        />
      </MantineProvider>,
    );

    fireEvent.change(screen.getByRole("textbox", { name: "Email address" }), {
      target: { value: "person@company.example" },
    });
    fireEvent.click(screen.getByRole("textbox", { name: "Provider" }));
    fireEvent.click(await screen.findByRole("option", { name: "Gmail" }));

    expect(
      screen.getByText("Connect Gmail with an app password"),
    ).toBeVisible();
  });

  it("removes Gmail-specific guidance after a manual provider override", async () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
        />
      </MantineProvider>,
    );

    fireEvent.change(screen.getByRole("textbox", { name: "Email address" }), {
      target: { value: "person@gmail.com" },
    });
    fireEvent.click(screen.getByRole("textbox", { name: "Provider" }));
    fireEvent.click(await screen.findByRole("option", { name: "Fastmail" }));

    expect(
      screen.queryByText("Connect Gmail with an app password"),
    ).not.toBeInTheDocument();
    expect(screen.getByLabelText("Password or app password")).toBeVisible();

    await settleComboboxUpdates();
  });
});
