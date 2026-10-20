import { describe, expect, it } from "vitest";
import {
  activePeopleSearchFilter,
  hasExplicitFolderPredicate,
  replacePeopleSearchFilter,
} from "./searchSyntax";

describe("hasExplicitFolderPredicate", () => {
  it("recognizes supported folder predicate values outside quoted text", () => {
    expect(hasExplicitFolderPredicate("in:Sent")).toBe(true);
    expect(hasExplicitFolderPredicate('in:"Projects/Client work"')).toBe(true);
    expect(hasExplicitFolderPredicate("in:*")).toBe(true);
    expect(hasExplicitFolderPredicate("(in:Spam OR in:Trash)")).toBe(true);
    expect(hasExplicitFolderPredicate("NOT in:Sent")).toBe(true);
  });

  it("does not mistake quoted content or longer field names for folder scope", () => {
    expect(hasExplicitFolderPredicate('subject:"in:Sent"')).toBe(false);
    expect(hasExplicitFolderPredicate('"in:Trash"')).toBe(false);
    expect(hasExplicitFolderPredicate("'note in:archive'")).toBe(false);
    expect(hasExplicitFolderPredicate("subject:'note in:archive'")).toBe(false);
    expect(
      hasExplicitFolderPredicate(String.raw`'note \'in:archive\\path'`),
    ).toBe(false);
    expect(hasExplicitFolderPredicate("within:Sent")).toBe(false);
    expect(hasExplicitFolderPredicate("in:")).toBe(false);
  });

  it("recognizes a single-quoted folder value", () => {
    expect(hasExplicitFolderPredicate("in:'Projects/Client work'")).toBe(true);
  });
});

describe("activePeopleSearchFilter", () => {
  it("finds an unfinished people filter at the end of an open Boolean group", () => {
    const value = "(from:al";
    const filter = activePeopleSearchFilter(value);
    expect(filter).toMatchObject({ field: "from", prefix: "al" });
    expect(replacePeopleSearchFilter(value, filter!, "alex@example.com")).toBe(
      "(from:alex@example.com",
    );
  });

  it("does not reuse a completed people predicate when a later token is active", () => {
    expect(activePeopleSearchFilter("from:alice subject:inv")).toBeUndefined();
    expect(
      activePeopleSearchFilter("(from:alice OR subject:invoice)"),
    ).toBeUndefined();
    expect(activePeopleSearchFilter('from:"Alice"')).toBeUndefined();
  });

  it("keeps an unfinished quoted person filter searchable and decodes escapes", () => {
    expect(activePeopleSearchFilter('from:"Alice S')).toMatchObject({
      field: "from",
      prefix: "Alice S",
    });
    expect(
      activePeopleSearchFilter('subject:"from:ignore" from:"A\\"l'),
    ).toMatchObject({
      field: "from",
      prefix: 'A"l',
    });
    expect(activePeopleSearchFilter("from:'Alice S")).toMatchObject({
      field: "from",
      prefix: "Alice S",
    });
    expect(activePeopleSearchFilter(String.raw`from:'A\'l\\x`)).toMatchObject({
      field: "from",
      prefix: "A'l\\x",
    });
  });

  it("does not offer people autocomplete for text inside either quote style", () => {
    expect(activePeopleSearchFilter("'from:ignore'")).toBeUndefined();
    expect(
      activePeopleSearchFilter("subject:'from:ignore' from:Al"),
    ).toMatchObject({ field: "from", prefix: "Al" });
    expect(activePeopleSearchFilter("from:'Alice'")).toBeUndefined();
  });
});
