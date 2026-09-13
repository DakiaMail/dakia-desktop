import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import { RecipientCombobox } from "./RecipientCombobox";

let contactedPeopleChanged:
  ((change: { enabled: boolean; cleared: boolean }) => void) | undefined;

const suggestions = [
  {
    address: "jane@example.com",
    display_name: "Jane Doe",
    last_contacted_at: "2026-09-01T00:00:00Z",
  },
  {
    address: "john@example.com",
    display_name: "John Doe",
    last_contacted_at: "2026-09-02T00:00:00Z",
  },
];

function Fixture({
  accountId = "account-a",
  disabled = false,
  excludedAddresses = new Set<string>(),
}: {
  accountId?: string;
  disabled?: boolean;
  excludedAddresses?: Set<string>;
}) {
  const [value, setValue] = useState("");
  return (
    <>
      <RecipientCombobox
        id="recipients"
        label="To"
        value={value}
        onChange={setValue}
        accountId={accountId}
        disabled={disabled}
        excludedAddresses={excludedAddresses}
      />
      <output data-testid="value">{value}</output>
    </>
  );
}

describe("RecipientCombobox", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(api, "contactedPeopleSettings").mockResolvedValue({
      enabled: true,
    });
    vi.spyOn(api, "suggestContactedPeople").mockResolvedValue(suggestions);
    vi.spyOn(api, "hideContactedPerson").mockResolvedValue();
    contactedPeopleChanged = undefined;
    vi.spyOn(api, "onContactedPeopleChanged").mockImplementation(
      async (handler) => {
        contactedPeopleChanged = handler;
        return () => undefined;
      },
    );
  });

  it("selects an account-ranked contacted person with the keyboard", async () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", {
      name: "To",
    }) as HTMLInputElement;
    fireEvent.focus(input);

    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(screen.getByTestId("value")).toHaveTextContent(
      "Jane Doe <jane@example.com>",
    );
    expect(api.suggestContactedPeople).toHaveBeenCalledWith("", "account-a");
  });

  it("keeps quoted commas, commits multi-address paste, and suppresses cross-field duplicates", async () => {
    render(<Fixture excludedAddresses={new Set(["taken@example.com"])} />);
    const input = screen.getByRole("combobox", { name: "To" });

    fireEvent.change(input, {
      target: { value: '"Doe, Jane" <jane@example.com>' },
    });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByTestId("value")).toHaveTextContent(
      '"Doe, Jane" <jane@example.com>',
    );

    fireEvent.paste(input, {
      clipboardData: {
        getData: () => "Other <other@example.com>, Taken <taken@example.com>",
      },
    });
    expect(screen.getByTestId("value")).toHaveTextContent(
      '"Doe, Jane" <jane@example.com>, Other <other@example.com>',
    );
    expect(
      document.querySelector(".recipient-combobox-status"),
    ).toHaveTextContent("That recipient is already included.");
  });

  it("removes an all-duplicate manual draft instead of leaving it for another recipient field", () => {
    render(<Fixture excludedAddresses={new Set(["taken@example.com"])} />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.change(input, { target: { value: "taken@example.com" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(input).toHaveValue("");
    expect(screen.getByTestId("value")).toHaveTextContent("");
    expect(
      document.querySelector(".recipient-combobox-status"),
    ).toHaveTextContent("That recipient is already included.");
  });

  it("pastes multiple addresses into the selected draft range without dropping surrounding text", () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", {
      name: "To",
    }) as HTMLInputElement;
    const initial = "First <first@example.test>, replace@example.test";
    fireEvent.change(input, { target: { value: initial } });
    input.setSelectionRange(
      initial.indexOf("replace@example.test"),
      initial.length,
    );

    fireEvent.paste(input, {
      clipboardData: {
        getData: () => "Second <second@example.test>, third@example.test",
      },
    });

    expect(screen.getByTestId("value")).toHaveTextContent(
      "First <first@example.test>, Second <second@example.test>, third@example.test",
    );
  });

  it("does not mark Rust-valid quoted-local or local-domain recipients as invalid", () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.change(input, { target: { value: '"quoted local"@localhost' } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(document.querySelector(".recipient-token")).not.toHaveAttribute(
      "data-invalid",
    );
  });

  it("retains malformed manual input and marks it before sending", () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.change(input, { target: { value: "not an email" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(document.querySelector(".recipient-token")).toHaveAttribute(
      "data-invalid",
      "true",
    );
    expect(screen.getByTestId("value")).toHaveTextContent("not an email");
    expect(
      document.querySelector(".recipient-combobox-status"),
    ).toHaveTextContent("looks incomplete");
  });

  it("reranks after the From account changes and hides the active option without nesting controls", async () => {
    const { rerender } = render(<Fixture accountId="account-a" />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    const jane = await screen.findByRole("option", { name: /Jane Doe/ });
    expect(within(jane).queryByRole("button")).toBeNull();

    rerender(<Fixture accountId="account-b" />);
    await waitFor(() =>
      expect(api.suggestContactedPeople).toHaveBeenCalledWith("", "account-b"),
    );
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(screen.getByRole("button", { name: /Hide Jane Doe/ })).toBeEnabled();
    fireEvent.keyDown(input, { key: "Delete", altKey: true });
    expect(api.hideContactedPerson).toHaveBeenCalledWith("jane@example.com");
    expect(screen.queryByRole("option", { name: /Jane Doe/ })).toBeNull();
    expect(
      document.querySelector(".recipient-combobox-status"),
    ).toHaveTextContent("Recipient hidden from suggestions.");
  });

  it("clears old account options before a delayed rerank can be selected", async () => {
    let resolveAccountB: ((items: typeof suggestions) => void) | undefined;
    vi.mocked(api.suggestContactedPeople).mockImplementation(
      (_prefix, accountId) =>
        accountId === "account-b"
          ? new Promise((resolve) => {
              resolveAccountB = resolve;
            })
          : Promise.resolve(suggestions),
    );
    const { rerender } = render(<Fixture accountId="account-a" />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();

    rerender(<Fixture accountId="account-b" />);
    await waitFor(() =>
      expect(api.suggestContactedPeople).toHaveBeenCalledWith("", "account-b"),
    );
    expect(screen.queryByRole("option", { name: /Jane Doe/ })).toBeNull();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Tab" });
    expect(screen.getByTestId("value")).toHaveTextContent("");
    expect(input).not.toHaveAttribute("aria-activedescendant");

    await act(async () => resolveAccountB?.([suggestions[1]]));
    expect(
      await screen.findByRole("option", { name: /John Doe/ }),
    ).toBeVisible();
  });

  it("supports keyboard, pointer, delimiter, and live-region recipient acceptance", async () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    const jane = await screen.findByRole("option", { name: /Jane Doe/ });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input).toHaveAttribute("aria-activedescendant", jane.id);
    fireEvent.keyDown(input, { key: "Tab" });
    expect(screen.getByTestId("value")).toHaveTextContent(
      "Jane Doe <jane@example.com>",
    );
    expect(document.querySelector('[role="status"]')).toHaveTextContent(
      "Jane Doe <jane@example.com> added.",
    );

    fireEvent.focus(input);
    expect(
      await screen.findByRole("option", { name: /John Doe/ }),
    ).toBeVisible();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(input).toHaveAttribute("aria-expanded", "false");

    fireEvent.focus(input);
    fireEvent.click(await screen.findByRole("option", { name: /John Doe/ }));
    fireEvent.change(input, { target: { value: "third@example.com" } });
    fireEvent.keyDown(input, { key: "," });
    fireEvent.change(input, { target: { value: "fourth@example.com" } });
    fireEvent.keyDown(input, { key: ";" });
    expect(screen.getByTestId("value")).toHaveTextContent(
      "Jane Doe <jane@example.com>, John Doe <john@example.com>, third@example.com, fourth@example.com",
    );
  });

  it("clears stale suggestions on another window's change and refetches only when enabled", async () => {
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();
    expect(contactedPeopleChanged).toBeTypeOf("function");
    const searchesBeforeDisable = vi.mocked(api.suggestContactedPeople).mock
      .calls.length;

    act(() => contactedPeopleChanged?.({ enabled: false, cleared: false }));
    expect(screen.queryByRole("option", { name: /Jane Doe/ })).toBeNull();
    await Promise.resolve();
    expect(api.suggestContactedPeople).toHaveBeenCalledTimes(
      searchesBeforeDisable,
    );

    act(() => contactedPeopleChanged?.({ enabled: true, cleared: true }));
    await waitFor(() =>
      expect(
        vi.mocked(api.suggestContactedPeople).mock.calls.length,
      ).toBeGreaterThan(searchesBeforeDisable),
    );
  });

  it("rechecks settings every time recipient input receives focus", async () => {
    const settings = vi.mocked(api.contactedPeopleSettings);
    settings.mockResolvedValueOnce({ enabled: true });
    render(<Fixture />);
    await waitFor(() => expect(settings).toHaveBeenCalled());
    settings.mockClear();
    settings.mockResolvedValueOnce({ enabled: false });
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);

    await waitFor(() => expect(settings).toHaveBeenCalledOnce());
    expect(api.suggestContactedPeople).not.toHaveBeenCalled();
  });

  it("restores a rejected optimistic hide in its original position with an accessible error", async () => {
    vi.mocked(api.hideContactedPerson).mockRejectedValueOnce(
      new Error("store unavailable"),
    );
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Delete", altKey: true });

    expect(screen.queryByRole("option", { name: /Jane Doe/ })).toBeNull();
    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();
    const restored = screen.getAllByRole("option");
    expect(restored).toHaveLength(2);
    expect(
      restored.filter((option) => option.textContent?.includes("Jane Doe")),
    ).toHaveLength(1);
    expect(restored[0]).toHaveTextContent("Jane Doe");
    expect(restored[1]).toHaveTextContent("John Doe");
    expect(
      document.querySelector(".recipient-combobox-status"),
    ).toHaveTextContent("Could not hide this recipient");
  });

  it("does not restore a rejected hide into a newer suggestion query", async () => {
    let rejectHide: (error: Error) => void = () => undefined;
    vi.mocked(api.hideContactedPerson).mockImplementationOnce(
      () =>
        new Promise<void>((_resolve, reject) => {
          rejectHide = reject;
        }),
    );
    vi.mocked(api.suggestContactedPeople).mockImplementation(async (prefix) =>
      prefix === "jo" ? [suggestions[1]] : suggestions,
    );
    render(<Fixture />);
    const input = screen.getByRole("combobox", { name: "To" });
    fireEvent.focus(input);
    expect(
      await screen.findByRole("option", { name: /Jane Doe/ }),
    ).toBeVisible();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Delete", altKey: true });
    fireEvent.change(input, { target: { value: "jo" } });
    expect(
      await screen.findByRole("option", { name: /John Doe/ }),
    ).toBeVisible();

    await act(async () => rejectHide(new Error("store unavailable")));

    expect(screen.queryByRole("option", { name: /Jane Doe/ })).toBeNull();
    expect(screen.getByRole("option", { name: /John Doe/ })).toBeVisible();
  });

  it("does not fetch or edit when disabled", () => {
    render(<Fixture disabled />);
    const input = screen.getByRole("combobox", { name: "To" });
    expect(input).toBeDisabled();
    fireEvent.focus(input);
    expect(api.suggestContactedPeople).not.toHaveBeenCalled();
  });
});
