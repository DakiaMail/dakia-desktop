/**
 * This is intentionally narrower than the Rust search parser. The desktop
 * needs only to know whether an `in:` predicate already establishes mailbox
 * scope, so it must not add the selected mailbox as another AND condition.
 */
export function hasExplicitFolderPredicate(rawQuery: string) {
  let quoted: '"' | "'" | false = false;
  let escaped = false;

  for (let index = 0; index < rawQuery.length; index += 1) {
    const character = rawQuery[index];
    if (quoted) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === quoted) {
        quoted = false;
      }
      continue;
    }
    const delimiter = quoteDelimiterAt(rawQuery, index);
    if (delimiter) {
      quoted = delimiter;
      continue;
    }
    if (
      rawQuery.slice(index, index + 3).toLowerCase() === "in:" &&
      isTermBoundary(rawQuery[index - 1]) &&
      hasFolderValue(rawQuery[index + 3])
    ) {
      return true;
    }
  }
  return false;
}

export type ActivePeopleSearchFilter = {
  field: "from" | "to" | "tonotcc" | "cc" | "bcc" | "with";
  /** The range of the complete filter, excluding a preceding unary + or -. */
  start: number;
  end: number;
  /** The value as the person index should search it. */
  prefix: string;
};

const peopleFields = new Set(["from", "to", "tonotcc", "cc", "bcc", "with"]);

/**
 * Finds the unfinished people predicate at the active end of the query
 * without treating text in a quoted phrase as syntax. Completed people terms
 * earlier in a Boolean expression are not autocomplete targets.
 */
export function activePeopleSearchFilter(
  rawQuery: string,
): ActivePeopleSearchFilter | undefined {
  let terminal: ActivePeopleSearchFilter | undefined;
  let index = 0;

  while (index < rawQuery.length) {
    const delimiter = quoteDelimiterAt(rawQuery, index);
    if (delimiter) {
      index = Math.min(
        skipQuoted(rawQuery, index + 1, delimiter) + 1,
        rawQuery.length,
      );
      continue;
    }
    if (!isTermBoundary(rawQuery[index - 1])) {
      index += 1;
      continue;
    }

    let fieldStart = index;
    if (rawQuery[fieldStart] === "+" || rawQuery[fieldStart] === "-") {
      fieldStart += 1;
    }
    const colon = rawQuery.indexOf(":", fieldStart);
    if (colon < 0) break;
    const field = rawQuery.slice(fieldStart, colon).toLowerCase();
    if (
      !peopleFields.has(field) ||
      /\s|[()]/.test(rawQuery.slice(fieldStart, colon))
    ) {
      index += 1;
      continue;
    }

    const valueStart = colon + 1;
    let end = valueStart;
    let prefix = "";
    const valueDelimiter = quotedDelimiter(rawQuery[valueStart]);
    if (valueDelimiter) {
      end = skipQuoted(rawQuery, valueStart + 1, valueDelimiter);
      const complete =
        end < rawQuery.length && rawQuery[end] === valueDelimiter;
      const rawValue = rawQuery.slice(valueStart + 1, end);
      prefix = unescapeSearchValue(rawValue);
      if (complete) {
        // A closing quote finishes this predicate. Do not offer a person
        // suggestion for a value the user has already completed.
        end += 1;
        index = Math.max(end, index + 1);
        continue;
      }
    } else {
      while (end < rawQuery.length && !/\s|[()]/.test(rawQuery[end])) end += 1;
      prefix = rawQuery.slice(valueStart, end);
    }
    const candidate = {
      field: field as ActivePeopleSearchFilter["field"],
      start: fieldStart,
      end,
      prefix,
    };
    if (end === rawQuery.length) terminal = candidate;
    index = Math.max(end, index + 1);
  }
  return terminal;
}

/** Replaces only the people predicate selected by activePeopleSearchFilter. */
export function replacePeopleSearchFilter(
  rawQuery: string,
  filter: ActivePeopleSearchFilter,
  replacement: string,
) {
  return `${rawQuery.slice(0, filter.start)}${filter.field}:${replacement}${rawQuery.slice(filter.end)}`;
}

function skipQuoted(value: string, start: number, delimiter: '"' | "'") {
  let escaped = false;
  for (let index = start; index < value.length; index += 1) {
    if (escaped) {
      escaped = false;
    } else if (value[index] === "\\") {
      escaped = true;
    } else if (value[index] === delimiter) {
      return index;
    }
  }
  return value.length;
}

function unescapeSearchValue(value: string) {
  return value.replace(/\\(['"\\])/g, "$1");
}

function quoteDelimiterAt(value: string, index: number): '"' | "'" | undefined {
  const character = value[index];
  if (character === '"') return character;
  if (character !== "'") return undefined;

  // Apostrophes stay inside ordinary unquoted words such as `don't`. They
  // delimit a quote only when they begin a term or follow a field colon.
  const previous = value[index - 1];
  if (previous === ":" || previous === undefined || /\s|[()]/.test(previous)) {
    return character;
  }
  if (previous === "+" || previous === "-") {
    const beforeUnary = value[index - 2];
    if (beforeUnary === undefined || /\s|[()]/.test(beforeUnary)) {
      return character;
    }
  }
  return undefined;
}

function quotedDelimiter(character: string | undefined): '"' | "'" | undefined {
  return character === '"' || character === "'" ? character : undefined;
}

function isTermBoundary(character: string | undefined) {
  return (
    character === undefined ||
    /\s/.test(character) ||
    character === "(" ||
    character === ")" ||
    character === "+" ||
    character === "-"
  );
}

function hasFolderValue(character: string | undefined) {
  return (
    character !== undefined &&
    !/\s/.test(character) &&
    character !== "(" &&
    character !== ")"
  );
}
