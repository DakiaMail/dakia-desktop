import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ComponentProps } from "react";
import "../i18n";
import { buildAdvancedSearchQuery, SearchControls } from "./SearchControls";

function renderSearch(
  overrides: Partial<ComponentProps<typeof SearchControls>> = {},
) {
  const props = {
    value: "",
    folderCatalogue: ["INBOX"],
    people: [],
    recentSearches: [],
    localOnly: false,
    onChange: vi.fn(),
    onSubmit: vi.fn(),
    onClearRecent: vi.fn(),
    onLocalOnlyChange: vi.fn(),
    ...overrides,
  };
  render(
    <MantineProvider>
      <SearchControls {...props} />
    </MantineProvider>,
  );
  return props;
}

describe("SearchControls", () => {
  it("keeps remote submission explicit while offering token-aware people", () => {
    const props = renderSearch({
      value: "from:al",
      people: [{ address: "alex@example.com", display_name: "Alex" }],
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });

    fireEvent.focus(input);
    expect(screen.getByText("Alex <alex@example.com>")).toBeVisible();
    fireEvent.click(screen.getByText("Alex <alex@example.com>"));
    expect(props.onChange).toHaveBeenCalledWith("from:alex@example.com");
    expect(props.onSubmit).toHaveBeenCalledWith("from:alex@example.com");

    fireEvent.keyDown(input, { key: "Enter" });
    expect(props.onSubmit).toHaveBeenCalledWith("from:al");
  });

  it("replaces a terminal people filter inside an open Boolean group and an unfinished quoted value", () => {
    const grouped = renderSearch({
      value: "(from:al",
      people: [{ address: "alex@example.com", display_name: "Alex" }],
    });
    const groupedInput = screen.getByRole("combobox", {
      name: "Search mail",
    });
    fireEvent.focus(groupedInput);
    fireEvent.click(screen.getByText("Alex <alex@example.com>"));
    expect(grouped.onChange).toHaveBeenCalledWith("(from:alex@example.com");

    const quoted = renderSearch({
      value: 'from:"Alice S',
      people: [{ address: "alice@example.com", display_name: "Alice Smith" }],
    });
    const quotedInput = screen.getAllByRole("combobox", {
      name: "Search mail",
    })[1];
    fireEvent.focus(quotedInput);
    fireEvent.click(screen.getByText("Alice Smith <alice@example.com>"));
    expect(quoted.onChange).toHaveBeenCalledWith("from:alice@example.com");
  });

  it("does not offer a person that could rewrite an earlier completed predicate", () => {
    const props = renderSearch({
      value: "from:alice subject:inv",
      people: [{ address: "bob@example.com", display_name: "Bob" }],
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);

    expect(screen.queryByText("Bob <bob@example.com>")).toBeNull();
    expect(props.onChange).not.toHaveBeenCalled();
  });

  it("never displays or replaces from people results bound to an earlier token", () => {
    const staleValue = "from:al to:bo";
    const stale = renderSearch({
      value: staleValue,
      people: [{ address: "alex@example.com", display_name: "Alex" }],
      peopleContext: {
        query: "from:al",
        filter: { field: "from", start: 0, end: 7, prefix: "al" },
        revision: 1,
      },
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    expect(screen.queryByText("Alex <alex@example.com>")).toBeNull();

    const current = renderSearch({
      value: staleValue,
      people: [{ address: "bob@example.com", display_name: "Bob" }],
      peopleContext: {
        query: staleValue,
        filter: { field: "to", start: 8, end: 13, prefix: "bo" },
        revision: 2,
      },
    });
    const currentInput = screen.getAllByRole("combobox", {
      name: "Search mail",
    })[1];
    fireEvent.focus(currentInput);
    fireEvent.click(screen.getByText("Bob <bob@example.com>"));
    expect(current.onChange).toHaveBeenCalledWith("from:al to:bob@example.com");
    expect(stale.onChange).not.toHaveBeenCalled();
  });

  it("shows recent contacted people when search is empty", () => {
    renderSearch({
      people: [{ address: "alex@example.com", display_name: "Alex" }],
    });
    fireEvent.focus(screen.getByRole("combobox", { name: "Search mail" }));
    expect(screen.getByText("Alex <alex@example.com>")).toBeVisible();
  });

  it("closes the dropdown with Escape before changing the query", () => {
    renderSearch();
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    expect(screen.getByText("Filters")).toBeVisible();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByText("Filters")).not.toBeInTheDocument();
  });

  it("uses arrow navigation before Enter submits the raw query", () => {
    const props = renderSearch();
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(props.onChange).toHaveBeenCalledWith("in:INBOX");
    expect(props.onSubmit).toHaveBeenCalledWith("in:INBOX");
  });

  it("selects the current non-Inbox folder as an accessible keyboard option", () => {
    const props = renderSearch({
      folderCatalogue: ["INBOX", "Projects/Client work"],
      people: [{ address: "alex@example.com", display_name: "Alex" }],
      recentSearches: ["subject:invoice"],
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });

    fireEvent.focus(input);
    for (let index = 0; index < 7; index += 1) {
      fireEvent.keyDown(input, { key: "ArrowDown" });
    }

    const folder = screen.getByRole("option", {
      name: "In Projects/Client work",
    });
    expect(folder).toHaveAttribute("aria-selected", "true");
    expect(input).toHaveAttribute("aria-activedescendant", folder.id);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(props.onChange).toHaveBeenCalledWith('in:"Projects/Client work"');
    expect(props.onSubmit).toHaveBeenCalledWith('in:"Projects/Client work"');
  });

  it("emits parser-valid filter and mailbox values", () => {
    const props = renderSearch({
      folderCatalogue: ["INBOX", "Projects/Client work"],
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByText("Unread"));
    expect(props.onChange).toHaveBeenCalledWith("is:unread");

    fireEvent.focus(input);
    fireEvent.click(screen.getByText("In Projects/Client work"));
    expect(props.onChange).toHaveBeenCalledWith('in:"Projects/Client work"');
  });

  it("builds an implicit-AND advanced query with folder, dates, state, and attachment filters", async () => {
    const props = renderSearch();
    const input = screen.getByRole("combobox", { name: "Search mail" });

    fireEvent.focus(input);
    fireEvent.click(screen.getByRole("button", { name: "Advanced search" }));
    fireEvent.change(await screen.findByLabelText("From"), {
      target: { value: "ada@example.com" },
    });
    fireEvent.change(screen.getByLabelText("Folder"), {
      target: { value: "Projects/Client work" },
    });
    fireEvent.change(screen.getByLabelText("After"), {
      target: { value: "2026-01-01" },
    });
    fireEvent.change(screen.getByLabelText("Before"), {
      target: { value: "2026-02-01" },
    });
    fireEvent.change(screen.getByLabelText("Read state"), {
      target: { value: "unread" },
    });
    fireEvent.change(screen.getByLabelText("Flagged or pinned"), {
      target: { value: "flagged" },
    });
    fireEvent.change(screen.getByLabelText("Attachment"), {
      target: { value: "attachment" },
    });
    fireEvent.change(screen.getByLabelText("File type"), {
      target: { value: "pdf" },
    });

    fireEvent.click(screen.getByRole("button", { name: "Apply search" }));
    const expected =
      'from:ada@example.com in:"Projects/Client work" after:2026-01-01 before:2026-02-01 is:unread is:flagged has:attachment filetype:pdf';
    expect(props.onChange).toHaveBeenCalledWith(expected);
    expect(props.onSubmit).toHaveBeenCalledWith(expected);
  });

  it("labels every advanced control and applies it with Enter", async () => {
    const props = renderSearch();
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByRole("button", { name: "Advanced search" }));

    const dialog = await screen.findByRole("dialog");
    const from = within(dialog).getByLabelText("From");
    expect(within(dialog).getByLabelText("To")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Cc")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Bcc")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("With")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Message body")).toBeInTheDocument();
    fireEvent.change(from, { target: { value: 'Ada "Lovelace"' } });
    fireEvent.keyDown(from, { key: "Enter" });

    expect(props.onSubmit).toHaveBeenCalledWith('from:"Ada \\"Lovelace\\""');
  });

  it("quotes values and omits empty advanced fields", () => {
    expect(
      buildAdvancedSearchQuery({
        from: "",
        to: "",
        cc: "",
        bcc: "",
        with: "",
        subject: 'annual "review"',
        body: "",
        folder: "",
        after: "",
        before: "",
        readState: "",
        flagState: "",
        attachmentState: "",
        filetype: "",
      }),
    ).toBe('subject:"annual \\"review\\""');
  });

  it("replaces partial attachment and state tokens without changing complete terms", () => {
    const attachment = renderSearch({ value: "from:alex has:att" });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByText("Has attachment"));
    expect(attachment.onChange).toHaveBeenCalledWith(
      "from:alex has:attachment",
    );
    expect(attachment.onSubmit).toHaveBeenCalledWith(
      "from:alex has:attachment",
    );

    const state = renderSearch({ value: "subject:receipt is:unr" });
    fireEvent.focus(
      screen.getAllByRole("combobox", { name: "Search mail" })[1],
    );
    fireEvent.click(screen.getByText("Unread"));
    expect(state.onChange).toHaveBeenCalledWith("subject:receipt is:unread");
    expect(state.onSubmit).toHaveBeenCalledWith("subject:receipt is:unread");
  });

  it("replaces a partial filter after Boolean grouping punctuation", () => {
    const props = renderSearch({ value: "(has:att" });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByText("Has attachment"));

    expect(props.onChange).toHaveBeenCalledWith("(has:attachment");
    expect(props.onSubmit).toHaveBeenCalledWith("(has:attachment");
  });

  it("replaces partial date and folder tokens with parser-valid suggestions", () => {
    const date = renderSearch({ value: "date:2026-0" });
    fireEvent.focus(screen.getByRole("combobox", { name: "Search mail" }));
    fireEvent.click(screen.getByText("This week"));
    expect(date.onChange).toHaveBeenCalledWith("after:7d");
    expect(date.onSubmit).toHaveBeenCalledWith("after:7d");

    const folder = renderSearch({
      value: 'in:"Projects/Cl',
      folderCatalogue: ["INBOX", "Projects/Client work"],
    });
    fireEvent.focus(
      screen.getAllByRole("combobox", { name: "Search mail" })[1],
    );
    fireEvent.click(screen.getByText("In Projects/Client work"));
    expect(folder.onChange).toHaveBeenCalledWith('in:"Projects/Client work"');
    expect(folder.onSubmit).toHaveBeenCalledWith('in:"Projects/Client work"');
  });

  it("suggests arbitrary catalogued nested folders with a quoted token", () => {
    const props = renderSearch({
      value: "in:client",
      folderCatalogue: [
        "INBOX",
        "Archive",
        "Projects/Client A",
        "Projects/Client B",
      ],
    });
    const input = screen.getByRole("combobox", { name: "Search mail" });

    fireEvent.focus(input);
    expect(
      screen.getByRole("option", { name: "In Projects/Client A" }),
    ).toBeVisible();
    expect(screen.queryByRole("option", { name: "In Archive" })).toBeNull();
    fireEvent.click(
      screen.getByRole("option", { name: "In Projects/Client A" }),
    );

    expect(props.onChange).toHaveBeenCalledWith('in:"Projects/Client A"');
    expect(props.onSubmit).toHaveBeenCalledWith('in:"Projects/Client A"');
  });

  it("does not overwrite a grouped Boolean query in advanced search", async () => {
    renderSearch({ value: "from:alex OR from:sam" });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByRole("button", { name: "Advanced search" }));
    expect(
      await screen.findByText(/grouped Boolean logic/i),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply search" })).toBeNull();
  });

  it("submits a recent search and clears history separately", () => {
    const props = renderSearch({ recentSearches: ["subject:invoice"] });
    const input = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(input);
    fireEvent.click(screen.getByText("subject:invoice"));
    expect(props.onChange).toHaveBeenCalledWith("subject:invoice");
    expect(props.onSubmit).toHaveBeenCalledWith("subject:invoice");
    fireEvent.focus(input);
    fireEvent.click(screen.getByRole("button", { name: "Clear all" }));
    expect(props.onClearRecent).toHaveBeenCalledOnce();
  });
});
