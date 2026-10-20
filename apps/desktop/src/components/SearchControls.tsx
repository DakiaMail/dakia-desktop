import { Button, Checkbox, Modal, TextInput } from "@mantine/core";
import {
  IconCalendarEvent,
  IconPaperclip,
  IconSearch,
  IconUser,
} from "@tabler/icons-react";
import {
  useEffect,
  useId,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import { useTranslation } from "react-i18next";
import {
  activePeopleSearchFilter,
  replacePeopleSearchFilter,
  type ActivePeopleSearchFilter,
} from "../searchSyntax";
import type { ContactedPersonSuggestion } from "../types";

export type SearchSuggestion = {
  address: string;
  displayName?: string | null;
};

/** Binds async people results to the exact search token that requested them. */
export type SearchPeopleSuggestionContext = {
  query: string;
  filter?: ActivePeopleSearchFilter;
  revision: number;
};

type Props = {
  value: string;
  folderCatalogue: string[];
  people: ContactedPersonSuggestion[];
  peopleContext?: SearchPeopleSuggestionContext;
  recentSearches: string[];
  localOnly: boolean;
  searchRef?: RefObject<HTMLInputElement | null>;
  onChange: (value: string) => void;
  onSubmit: (value: string) => void;
  onClearRecent: () => void;
  onLocalOnlyChange: (enabled: boolean) => void;
  onSaveSearch?: () => void;
};

const complexBoolean = /\b(?:OR|NOT)\b|[()]/;

function displayAddress(person: ContactedPersonSuggestion) {
  if (person.formatted_address) return person.formatted_address;
  return person.display_name
    ? `${person.display_name} <${person.address}>`
    : person.address;
}

function appendTerm(value: string, term: string) {
  return value.trim() ? `${value.trim()} ${term}` : term;
}

function activeTokenBounds(value: string) {
  let start = 0;
  let quoted = false;
  let escaped = false;
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (quoted) {
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === '"') quoted = false;
    } else if (character === '"') {
      quoted = true;
    } else if (/\s/.test(character) || character === "(" || character === ")") {
      start = index + 1;
    }
  }
  return { start, token: value.slice(start) };
}

/** Replace an unfinished filter, while preserving every preceding complete term. */
function replaceActiveFilterToken(value: string, replacement: string) {
  const active = activeTokenBounds(value);
  if (/^(?:has|filetype|after|before|date|is|in):/i.test(active.token)) {
    return `${value.slice(0, active.start)}${replacement}`;
  }
  return appendTerm(value, replacement);
}

export function quoteSearchValue(value: string) {
  return /[\s"\\]/.test(value)
    ? `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`
    : value;
}

export type AdvancedSearchValues = {
  from: string;
  to: string;
  cc: string;
  bcc: string;
  with: string;
  subject: string;
  body: string;
  folder: string;
  after: string;
  before: string;
  readState: "" | "read" | "unread";
  flagState: "" | "flagged" | "unflagged";
  attachmentState: "" | "attachment" | "noattachment";
  filetype: string;
};

const emptyAdvancedSearch: AdvancedSearchValues = {
  from: "",
  to: "",
  cc: "",
  bcc: "",
  with: "",
  subject: "",
  body: "",
  folder: "",
  after: "",
  before: "",
  readState: "",
  flagState: "",
  attachmentState: "",
  filetype: "",
};

/** Builds one parser-valid, implicit-AND query from the advanced form. */
export function buildAdvancedSearchQuery(values: AdvancedSearchValues) {
  const textTerms = (
    [
      ["from", values.from],
      ["to", values.to],
      ["cc", values.cc],
      ["bcc", values.bcc],
      ["with", values.with],
      ["subject", values.subject],
      ["body", values.body],
      ["in", values.folder],
      ["after", values.after],
      ["before", values.before],
    ] as const
  ).flatMap(([field, rawValue]) => {
    const fieldValue = rawValue.trim();
    return fieldValue ? [`${field}:${quoteSearchValue(fieldValue)}`] : [];
  });
  const exactTerms = [
    values.readState && `is:${values.readState}`,
    values.flagState && `is:${values.flagState}`,
    values.attachmentState && `has:${values.attachmentState}`,
    values.filetype && `filetype:${values.filetype}`,
  ].filter((term): term is string => Boolean(term));
  return [...textTerms, ...exactTerms].join(" ");
}

export function SearchControls({
  value,
  folderCatalogue,
  people,
  peopleContext,
  recentSearches,
  localOnly,
  searchRef,
  onChange,
  onSubmit,
  onClearRecent,
  onLocalOnlyChange,
  onSaveSearch,
}: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [advancedValues, setAdvancedValues] =
    useState<AdvancedSearchValues>(emptyAdvancedSearch);
  const listId = useId();
  const personField = activePeopleSearchFilter(value);
  const peopleAreCurrent =
    !peopleContext ||
    (peopleContext.query === value &&
      samePeopleFilter(peopleContext.filter, personField));
  const visiblePeople =
    peopleAreCurrent && (personField || !value.trim()) ? people : [];
  const tokenOptions = [
    { label: t("search.inbox"), value: "in:INBOX", icon: IconSearch },
    {
      label: t("search.hasAttachment"),
      value: "has:attachment",
      icon: IconPaperclip,
    },
    {
      label: t("search.calendar"),
      value: "filetype:calendar",
      icon: IconCalendarEvent,
    },
    { label: t("search.unread"), value: "is:unread", icon: IconSearch },
    { label: t("search.thisWeek"), value: "after:7d", icon: IconCalendarEvent },
  ];
  const folderOptions = catalogueFolderOptions(folderCatalogue, value);
  const folderStartIndex = visiblePeople.length + tokenOptions.length;
  const selectableCount =
    visiblePeople.length +
    tokenOptions.length +
    folderOptions.length +
    recentSearches.length;
  const optionId = (index: number) => `${listId}-option-${index}`;

  const selectPerson = (person: ContactedPersonSuggestion) =>
    choose(
      personField
        ? replacePeopleSearchFilter(
            value,
            peopleContext?.filter ?? personField,
            quoteSearchValue(person.address),
          )
        : appendTerm(value, `from:${quoteSearchValue(person.address)}`),
      true,
    );

  const selectActive = () => {
    if (activeIndex < 0) return false;
    if (activeIndex < visiblePeople.length) {
      selectPerson(visiblePeople[activeIndex]);
      return true;
    }
    const tokenIndex = activeIndex - visiblePeople.length;
    if (tokenIndex < tokenOptions.length) {
      choose(
        replaceActiveFilterToken(value, tokenOptions[tokenIndex].value),
        true,
      );
      return true;
    }
    const folder = folderOptions[tokenIndex - tokenOptions.length];
    if (folder) {
      choose(
        replaceActiveFilterToken(value, `in:${quoteSearchValue(folder)}`),
        true,
      );
      return true;
    }
    const recent =
      recentSearches[tokenIndex - tokenOptions.length - folderOptions.length];
    if (recent) {
      choose(recent, true);
      return true;
    }
    return false;
  };

  useEffect(() => setActiveIndex(-1), [value, open]);

  const choose = (next: string, submit = false) => {
    onChange(next);
    setOpen(false);
    if (submit) onSubmit(next);
  };
  const submit = () => {
    onSubmit(value);
    setOpen(false);
  };
  const updateAdvancedValue = <Key extends keyof AdvancedSearchValues>(
    field: Key,
    nextValue: AdvancedSearchValues[Key],
  ) => {
    setAdvancedValues((current) => ({ ...current, [field]: nextValue }));
  };
  const applyAdvancedSearch = () => {
    const advancedQuery = buildAdvancedSearchQuery(advancedValues);
    if (!advancedQuery) return;
    const next = appendTerm(value, advancedQuery);
    onChange(next);
    onSubmit(next);
    setAdvancedOpen(false);
  };

  return (
    <>
      <div className="mail-search" data-open={open || undefined}>
        <TextInput
          ref={searchRef}
          value={value}
          onFocus={() => setOpen(true)}
          onChange={(event) => onChange(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              if (open) {
                event.preventDefault();
                setOpen(false);
              }
              return;
            }
            if (event.key === "ArrowDown" && selectableCount) {
              event.preventDefault();
              setOpen(true);
              setActiveIndex((current) =>
                Math.min(current + 1, selectableCount - 1),
              );
              return;
            }
            if (event.key === "ArrowUp" && selectableCount) {
              event.preventDefault();
              setActiveIndex((current) => Math.max(current - 1, 0));
              return;
            }
            if (event.key === "Enter") {
              event.preventDefault();
              if (open && selectActive()) return;
              submit();
            }
          }}
          leftSection={<IconSearch size={16} />}
          placeholder={t("search.placeholder")}
          aria-label={t("actions.search")}
          role="combobox"
          aria-expanded={open}
          aria-controls={listId}
          aria-activedescendant={
            activeIndex >= 0 ? optionId(activeIndex) : undefined
          }
          aria-autocomplete="list"
        />
        {open ? (
          <div className="search-dropdown" id={listId} role="listbox">
            {visiblePeople.length ? (
              <SearchGroup label={t("search.people")}>
                {visiblePeople.map((person, index) => (
                  <button
                    key={person.address}
                    id={optionId(index)}
                    className="search-dropdown-item"
                    role="option"
                    aria-selected={activeIndex === index}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => selectPerson(person)}
                  >
                    <IconUser size={15} />
                    <span>{displayAddress(person)}</span>
                  </button>
                ))}
              </SearchGroup>
            ) : null}
            <SearchGroup label={t("search.filters")}>
              {tokenOptions.map(
                ({ label, value: token, icon: Icon }, index) => (
                  <button
                    key={token}
                    id={optionId(visiblePeople.length + index)}
                    className="search-dropdown-item"
                    role="option"
                    aria-selected={activeIndex === visiblePeople.length + index}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() =>
                      choose(replaceActiveFilterToken(value, token), true)
                    }
                  >
                    <Icon size={15} />
                    <span>{label}</span>
                  </button>
                ),
              )}
            </SearchGroup>
            {folderOptions.length ? (
              <SearchGroup label={t("search.folders")}>
                {folderOptions.map((folder, index) => {
                  const optionIndex = folderStartIndex + index;
                  return (
                    <button
                      key={folder}
                      id={optionId(optionIndex)}
                      className="search-dropdown-item"
                      role="option"
                      aria-selected={activeIndex === optionIndex}
                      onMouseDown={(event) => event.preventDefault()}
                      onClick={() =>
                        choose(
                          replaceActiveFilterToken(
                            value,
                            `in:${quoteSearchValue(folder)}`,
                          ),
                          true,
                        )
                      }
                    >
                      <IconSearch size={15} />
                      <span>{t("search.folder", { folder })}</span>
                    </button>
                  );
                })}
              </SearchGroup>
            ) : null}
            {recentSearches.length ? (
              <SearchGroup
                label={t("search.recent")}
                action={
                  <button className="search-clear" onClick={onClearRecent}>
                    {t("search.clearAll")}
                  </button>
                }
              >
                {recentSearches.map((search, index) => (
                  <button
                    key={search}
                    id={optionId(
                      visiblePeople.length +
                        tokenOptions.length +
                        folderOptions.length +
                        index,
                    )}
                    className="search-dropdown-item"
                    role="option"
                    aria-selected={
                      activeIndex ===
                      visiblePeople.length +
                        tokenOptions.length +
                        folderOptions.length +
                        index
                    }
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => choose(search, true)}
                  >
                    <IconSearch size={15} />
                    <span>{search}</span>
                  </button>
                ))}
              </SearchGroup>
            ) : null}
            <div className="search-dropdown-footer">
              <Checkbox
                checked={localOnly}
                onChange={(event) =>
                  onLocalOnlyChange(event.currentTarget.checked)
                }
                label={t("search.localOnlySetting")}
                size="xs"
              />
              <button
                className="search-advanced-button"
                onClick={() => {
                  setOpen(false);
                  setAdvancedOpen(true);
                }}
              >
                {t("search.advanced")}
              </button>
              {value.trim() ? (
                <button
                  className="search-advanced-button"
                  onClick={onSaveSearch}
                >
                  {t("search.save")}
                </button>
              ) : null}
            </div>
          </div>
        ) : null}
      </div>
      <Modal
        opened={advancedOpen}
        onClose={() => setAdvancedOpen(false)}
        title={t("search.advanced")}
        centered
      >
        {complexBoolean.test(value) ? (
          <p className="search-advanced-notice">
            {t("search.advancedComplex")}
          </p>
        ) : (
          <form
            className="search-advanced-fields"
            onSubmit={(event) => {
              event.preventDefault();
              applyAdvancedSearch();
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                applyAdvancedSearch();
              }
            }}
          >
            <fieldset>
              <legend>{t("search.advancedPeople")}</legend>
              <div className="search-advanced-grid">
                {(
                  [
                    ["from", "fieldFrom"],
                    ["to", "fieldTo"],
                    ["cc", "fieldCc"],
                    ["bcc", "fieldBcc"],
                    ["with", "fieldWith"],
                  ] as const
                ).map(([field, label]) => (
                  <TextInput
                    key={field}
                    label={t(`search.${label}`)}
                    value={advancedValues[field]}
                    onChange={(event) =>
                      updateAdvancedValue(field, event.currentTarget.value)
                    }
                  />
                ))}
              </div>
            </fieldset>
            <fieldset>
              <legend>{t("search.advancedContent")}</legend>
              <div className="search-advanced-grid">
                <TextInput
                  label={t("search.fieldSubject")}
                  value={advancedValues.subject}
                  onChange={(event) =>
                    updateAdvancedValue("subject", event.currentTarget.value)
                  }
                />
                <TextInput
                  label={t("search.fieldBody")}
                  value={advancedValues.body}
                  onChange={(event) =>
                    updateAdvancedValue("body", event.currentTarget.value)
                  }
                />
              </div>
            </fieldset>
            <fieldset>
              <legend>{t("search.advancedLocationAndDate")}</legend>
              <div className="search-advanced-grid">
                <TextInput
                  label={t("search.fieldFolder")}
                  value={advancedValues.folder}
                  onChange={(event) =>
                    updateAdvancedValue("folder", event.currentTarget.value)
                  }
                />
                <TextInput
                  label={t("search.fieldAfter")}
                  placeholder={t("search.dateExample")}
                  value={advancedValues.after}
                  onChange={(event) =>
                    updateAdvancedValue("after", event.currentTarget.value)
                  }
                />
                <TextInput
                  label={t("search.fieldBefore")}
                  placeholder={t("search.dateExample")}
                  value={advancedValues.before}
                  onChange={(event) =>
                    updateAdvancedValue("before", event.currentTarget.value)
                  }
                />
              </div>
            </fieldset>
            <fieldset>
              <legend>{t("search.advancedMessageState")}</legend>
              <div className="search-advanced-grid">
                <AdvancedSelect
                  label={t("search.fieldReadState")}
                  value={advancedValues.readState}
                  onChange={(next) =>
                    updateAdvancedValue(
                      "readState",
                      next as AdvancedSearchValues["readState"],
                    )
                  }
                  options={[
                    ["", t("search.any")],
                    ["read", t("search.read")],
                    ["unread", t("search.unread")],
                  ]}
                />
                <AdvancedSelect
                  label={t("search.fieldFlagState")}
                  value={advancedValues.flagState}
                  onChange={(next) =>
                    updateAdvancedValue(
                      "flagState",
                      next as AdvancedSearchValues["flagState"],
                    )
                  }
                  options={[
                    ["", t("search.any")],
                    ["flagged", t("search.flagged")],
                    ["unflagged", t("search.unflagged")],
                  ]}
                />
                <AdvancedSelect
                  label={t("search.fieldAttachmentState")}
                  value={advancedValues.attachmentState}
                  onChange={(next) =>
                    updateAdvancedValue(
                      "attachmentState",
                      next as AdvancedSearchValues["attachmentState"],
                    )
                  }
                  options={[
                    ["", t("search.any")],
                    ["attachment", t("search.hasAttachment")],
                    ["noattachment", t("search.hasNoAttachment")],
                  ]}
                />
                <AdvancedSelect
                  label={t("search.fieldFiletype")}
                  value={advancedValues.filetype}
                  onChange={(next) => updateAdvancedValue("filetype", next)}
                  options={[
                    ["", t("search.any")],
                    ["pdf", "PDF"],
                    ["document", t("search.filetypeDocument")],
                    ["spreadsheet", t("search.filetypeSpreadsheet")],
                    ["presentation", t("search.filetypePresentation")],
                    ["image", t("search.filetypeImage")],
                    ["audio", t("search.filetypeAudio")],
                    ["video", t("search.filetypeVideo")],
                    ["archive", t("search.filetypeArchive")],
                    ["calendar", t("search.filetypeCalendar")],
                  ]}
                />
              </div>
            </fieldset>
            <Button
              type="submit"
              disabled={!buildAdvancedSearchQuery(advancedValues)}
            >
              {t("search.apply")}
            </Button>
          </form>
        )}
      </Modal>
    </>
  );
}

function catalogueFolderOptions(folders: string[], value: string) {
  const token = activeTokenBounds(value).token;
  const match = /^in:(.*)$/i.exec(token);
  const filter = match?.[1].trim().replace(/^"/, "").toLocaleLowerCase();
  return [...new Set(folders.map((folder) => folder.trim()).filter(Boolean))]
    .filter((folder) => folder !== "INBOX")
    .filter((folder) => !filter || folder.toLocaleLowerCase().includes(filter))
    .sort((left, right) => left.localeCompare(right));
}

function samePeopleFilter(
  left: ActivePeopleSearchFilter | undefined,
  right: ActivePeopleSearchFilter | undefined,
) {
  if (!left || !right) return left === right;
  return (
    left.field === right.field &&
    left.start === right.start &&
    left.end === right.end &&
    left.prefix === right.prefix
  );
}

function AdvancedSelect({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  options: ReadonlyArray<readonly [string, string]>;
}) {
  const id = useId();
  return (
    <label htmlFor={id}>
      <span>{label}</span>
      <select
        id={id}
        value={value}
        onChange={(event) => onChange(event.currentTarget.value)}
      >
        {options.map(([optionValue, optionLabel]) => (
          <option key={optionValue} value={optionValue}>
            {optionLabel}
          </option>
        ))}
      </select>
    </label>
  );
}

function SearchGroup({
  label,
  action,
  children,
}: {
  label: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="search-dropdown-group">
      <div className="search-dropdown-heading">
        <span>{label}</span>
        {action}
      </div>
      {children}
    </section>
  );
}
