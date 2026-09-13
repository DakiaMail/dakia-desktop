import { IconX } from "@tabler/icons-react";
import {
  type ClipboardEvent,
  type KeyboardEvent,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api";
import {
  formatAddress,
  hasTrailingRecipientDelimiter,
  isValidRecipientValue,
  recipientAddressIdentity,
  splitAddressValues,
  type MailAddress,
} from "../recipients";
import type { ContactedPersonSuggestion } from "../types";

type Props = {
  id: string;
  label: string;
  value: string;
  accountId?: string;
  disabled?: boolean;
  autoFocus?: boolean;
  error?: string;
  excludedAddresses: ReadonlySet<string>;
  onChange: (value: string) => void;
};

/**
 * A small token input deliberately backed by the existing recipient parser.
 * Its value remains a normal RFC-style header string, so composing and sending
 * preserve the established backend contract.
 */
export function RecipientCombobox({
  id,
  label,
  value,
  accountId,
  disabled = false,
  autoFocus = false,
  error,
  excludedAddresses,
  onChange,
}: Props) {
  const { t } = useTranslation();
  const listboxId = useId();
  const errorId = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const lastPublishedRef = useRef(value);
  const requestRef = useRef(0);
  const settingsRequestRef = useRef(0);
  const suggestionsGenerationRef = useRef(0);
  const draftRef = useRef("");
  const [recipients, setRecipients] = useState(() => splitAddressValues(value));
  const [draft, setDraft] = useState("");
  const [open, setOpen] = useState(false);
  const [suggestions, setSuggestions] = useState<ContactedPersonSuggestion[]>(
    [],
  );
  const [suggestionsAccountId, setSuggestionsAccountId] = useState<
    string | undefined
  >();
  const [activeIndex, setActiveIndex] = useState(-1);
  const [suggestionsEnabled, setSuggestionsEnabled] = useState(true);
  const [settingsRevision, setSettingsRevision] = useState(0);
  const [settingsRefreshing, setSettingsRefreshing] = useState(true);
  const [status, setStatus] = useState("");

  // A From change changes the native ranking scope. Clear in a layout effect
  // so an option from the old account cannot be selected before the new
  // asynchronous request resolves.
  useLayoutEffect(() => {
    requestRef.current += 1;
    suggestionsGenerationRef.current += 1;
    setSuggestions([]);
    setSuggestionsAccountId(undefined);
    setActiveIndex(-1);
  }, [accountId]);

  useEffect(() => {
    if (value === lastPublishedRef.current) return;
    setRecipients(splitAddressValues(value));
    draftRef.current = "";
    setDraft("");
    lastPublishedRef.current = value;
  }, [value]);

  useEffect(() => {
    draftRef.current = draft;
  }, [draft]);

  const invalidateSuggestions = useCallback(() => {
    requestRef.current += 1;
    suggestionsGenerationRef.current += 1;
    setSuggestions([]);
    setSuggestionsAccountId(undefined);
    setActiveIndex(-1);
  }, []);

  const refreshSettings = useCallback(async () => {
    const request = ++settingsRequestRef.current;
    invalidateSuggestions();
    setSettingsRefreshing(true);
    try {
      const settings = await api.contactedPeopleSettings();
      if (request !== settingsRequestRef.current) return;
      setSuggestionsEnabled(settings.enabled);
      if (settings.enabled) setSettingsRevision((revision) => revision + 1);
    } catch {
      if (request !== settingsRequestRef.current) return;
      // Autocomplete is a convenience. Keep composition available if an older
      // native binary does not yet provide this optional setting command.
      setSuggestionsEnabled(true);
      setSettingsRevision((revision) => revision + 1);
    } finally {
      if (request === settingsRequestRef.current) setSettingsRefreshing(false);
    }
  }, [invalidateSuggestions]);

  useEffect(() => {
    void refreshSettings();
  }, [refreshSettings]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void api
      .onContactedPeopleChanged((change) => {
        settingsRequestRef.current += 1;
        invalidateSuggestions();
        setSuggestionsEnabled(change.enabled);
        setSettingsRefreshing(false);
        if (change.enabled) setSettingsRevision((revision) => revision + 1);
      })
      .then((dispose) => {
        if (disposed) dispose();
        else unlisten = dispose;
      })
      .catch(() => {
        // Focus rechecks settings when this newer event is unavailable.
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [invalidateSuggestions]);

  const usedAddresses = useMemo(() => {
    const used = new Set(excludedAddresses);
    for (const recipient of recipients) {
      const identity = recipientAddressIdentity(recipient);
      if (identity) used.add(identity);
    }
    return used;
  }, [excludedAddresses, recipients]);

  useEffect(() => {
    const request = ++requestRef.current;
    const generation = ++suggestionsGenerationRef.current;
    if (!open || disabled || !suggestionsEnabled || settingsRefreshing) {
      setSuggestions([]);
      setSuggestionsAccountId(undefined);
      setActiveIndex(-1);
      return;
    }
    void api
      .suggestContactedPeople(draft, accountId)
      .then((items) => {
        if (
          request !== requestRef.current ||
          generation !== suggestionsGenerationRef.current
        )
          return;
        const visible = items
          .filter((item) => {
            const identity = recipientAddressIdentity(item.address);
            return Boolean(
              identity && !item.hidden && !usedAddresses.has(identity),
            );
          })
          .slice(0, 8);
        setSuggestions(visible);
        setSuggestionsAccountId(accountId);
        setActiveIndex((index) =>
          visible.length ? Math.min(index, visible.length - 1) : -1,
        );
      })
      .catch(() => {
        if (
          request === requestRef.current &&
          generation === suggestionsGenerationRef.current
        ) {
          setSuggestions([]);
          setSuggestionsAccountId(undefined);
        }
      });
  }, [
    accountId,
    disabled,
    draft,
    open,
    settingsRevision,
    settingsRefreshing,
    suggestionsEnabled,
    usedAddresses,
  ]);

  const publish = (nextRecipients: string[], nextDraft = draft) => {
    const next = [...nextRecipients, nextDraft.trim()]
      .filter(Boolean)
      .join(", ");
    lastPublishedRef.current = next;
    onChange(next);
  };

  const commitValues = (values: string[]) => {
    const accepted: string[] = [];
    let duplicate = false;
    const nextUsed = new Set(usedAddresses);
    for (const rawValue of values) {
      const candidate = rawValue.trim();
      if (!candidate) continue;
      const identity = recipientAddressIdentity(candidate);
      if (identity && nextUsed.has(identity)) {
        duplicate = true;
        continue;
      }
      if (identity) nextUsed.add(identity);
      accepted.push(candidate);
    }
    if (!accepted.length) {
      if (duplicate) {
        // This may be a recipient currently being typed in another field.
        // Clear this field's duplicate draft so it cannot reach the
        // authoritative sender as a second To/Cc/Bcc recipient.
        setDraft("");
        draftRef.current = "";
        setOpen(false);
        setActiveIndex(-1);
        setStatus(t("composer.recipientDuplicate"));
        publish(recipients, "");
      }
      return;
    }
    const next = [...recipients, ...accepted];
    setRecipients(next);
    draftRef.current = "";
    setDraft("");
    setOpen(false);
    setActiveIndex(-1);
    setStatus(
      accepted.some((candidate) => !isValidRecipientValue(candidate))
        ? t("composer.recipientInvalid")
        : duplicate
          ? t("composer.recipientDuplicate")
          : "",
    );
    publish(next, "");
  };

  const commitDraft = () => {
    const values = splitAddressValues(draft);
    if (!values.length && draft.trim()) values.push(draft.trim());
    commitValues(values);
  };

  const selectSuggestion = (suggestion: ContactedPersonSuggestion) => {
    const formatted = formatSuggestion(suggestion);
    commitValues([formatted]);
    setStatus(t("composer.recipientAdded", { recipient: formatted }));
    inputRef.current?.focus();
  };

  const removeRecipient = (index: number) => {
    const next = recipients.filter((_, currentIndex) => currentIndex !== index);
    setRecipients(next);
    setStatus("");
    publish(next);
    inputRef.current?.focus();
  };

  const hideSuggestion = (suggestion: ContactedPersonSuggestion) => {
    const generation = suggestionsGenerationRef.current;
    const request = requestRef.current;
    const sourceDraft = draftRef.current;
    let removedIndex = -1;
    setSuggestions((items) =>
      items.filter((item, index) => {
        if (item.address !== suggestion.address) return true;
        removedIndex = index;
        return false;
      }),
    );
    void api.hideContactedPerson(suggestion.address).catch(() => {
      const stillShowingSameQuery =
        suggestionsGenerationRef.current === generation &&
        requestRef.current === request &&
        draftRef.current === sourceDraft;
      if (!stillShowingSameQuery || removedIndex < 0) {
        // A newer query may already have supplied a different suggestion set.
        // Fetch it again rather than putting an old option back into it.
        setSettingsRevision((revision) => revision + 1);
        return;
      }
      setSuggestions((items) => {
        if (items.some((item) => item.address === suggestion.address))
          return items;
        const restored = [...items];
        restored.splice(Math.min(removedIndex, restored.length), 0, suggestion);
        return restored;
      });
      setStatus(t("composer.recipientHideFailed"));
    });
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    const suggestionsReady = suggestionsAccountId === accountId;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setOpen(true);
      if (suggestionsReady) {
        setActiveIndex((index) => Math.min(index + 1, suggestions.length - 1));
      }
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((index) => Math.max(index - 1, 0));
      return;
    }
    if (event.key === "Escape") {
      setOpen(false);
      setActiveIndex(-1);
      return;
    }
    if (event.altKey && event.key === "Delete" && suggestions[activeIndex]) {
      event.preventDefault();
      hideSuggestion(suggestions[activeIndex]);
      setStatus(t("composer.recipientHidden"));
      return;
    }
    if (event.key === "Enter" || event.key === "Tab") {
      if (
        open &&
        suggestionsReady &&
        activeIndex >= 0 &&
        suggestions[activeIndex]
      ) {
        event.preventDefault();
        selectSuggestion(suggestions[activeIndex]);
      } else if (draft.trim()) {
        if (event.key === "Enter") event.preventDefault();
        commitDraft();
      }
      return;
    }
    if (
      (event.key === "," || event.key === ";") &&
      hasTrailingRecipientDelimiter(`${draft}${event.key}`)
    ) {
      event.preventDefault();
      commitValues(splitAddressValues(`${draft}${event.key}`));
      return;
    }
    if (event.key === "Backspace" && !draft && recipients.length) {
      removeRecipient(recipients.length - 1);
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLInputElement>) => {
    const pasted = event.clipboardData.getData("text");
    const values = splitAddressValues(pasted);
    if (values.length > 1 || hasTrailingRecipientDelimiter(pasted)) {
      event.preventDefault();
      // Preventing the native paste used to throw away the current draft.
      // Recreate the browser's replacement first, so text on both sides of a
      // selected range remains part of the recipient list.
      const input = event.currentTarget;
      const start = input.selectionStart ?? draft.length;
      const end = input.selectionEnd ?? start;
      const combined = `${draft.slice(0, start)}${pasted}${draft.slice(end)}`;
      const combinedValues = splitAddressValues(combined);
      commitValues(combinedValues.length ? combinedValues : values);
    }
  };

  const showListbox =
    open &&
    suggestionsEnabled &&
    suggestionsAccountId === accountId &&
    suggestions.length > 0;
  return (
    <div className="recipient-combobox">
      <div
        className="recipient-combobox-input"
        onClick={() => inputRef.current?.focus()}
      >
        {recipients.map((recipient, index) => (
          <span
            className="recipient-token"
            data-invalid={!isValidRecipientValue(recipient) || undefined}
            key={`${recipient}-${index}`}
          >
            <span>{recipient}</span>
            <button
              type="button"
              onClick={() => removeRecipient(index)}
              disabled={disabled}
              aria-label={t("composer.removeRecipient", { recipient })}
            >
              <IconX size={13} stroke={2} aria-hidden="true" />
            </button>
          </span>
        ))}
        <input
          ref={inputRef}
          id={id}
          value={draft}
          onChange={(event) => {
            const next = event.currentTarget.value;
            draftRef.current = next;
            setDraft(next);
            setOpen(true);
            setActiveIndex(-1);
            setStatus("");
            publish(recipients, next);
          }}
          onFocus={() => {
            setOpen(true);
            void refreshSettings();
          }}
          onBlur={() => window.setTimeout(() => setOpen(false), 120)}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
          role="combobox"
          aria-label={label}
          aria-autocomplete="list"
          aria-controls={showListbox ? listboxId : undefined}
          aria-expanded={showListbox}
          aria-activedescendant={
            activeIndex >= 0 && suggestions[activeIndex]
              ? `${listboxId}-${activeIndex}`
              : undefined
          }
          aria-invalid={Boolean(error) || undefined}
          aria-describedby={error ? errorId : undefined}
          autoFocus={autoFocus}
          autoComplete="off"
          spellCheck={false}
          disabled={disabled}
        />
      </div>
      {showListbox ? (
        <div className="recipient-suggestions">
          <ul
            id={listboxId}
            role="listbox"
            aria-label={t("composer.recipientSuggestions")}
          >
            {suggestions.map((suggestion, index) => (
              <li
                id={`${listboxId}-${index}`}
                key={suggestion.address}
                role="option"
                aria-selected={index === activeIndex}
                onMouseDown={(event) => event.preventDefault()}
                onMouseEnter={() => setActiveIndex(index)}
                onClick={() => selectSuggestion(suggestion)}
              >
                <span className="recipient-suggestion-select">
                  <strong>
                    {suggestion.display_name || suggestion.address}
                  </strong>
                  {suggestion.display_name ? (
                    <span>{suggestion.address}</span>
                  ) : null}
                </span>
              </li>
            ))}
          </ul>
          <button
            type="button"
            className="recipient-suggestion-hide"
            disabled={activeIndex < 0 || !suggestions[activeIndex]}
            aria-label={
              suggestions[activeIndex]
                ? t("composer.hideRecipient", {
                    recipient: formatSuggestion(suggestions[activeIndex]),
                  })
                : t("composer.hideSelectedRecipient")
            }
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => {
              if (!suggestions[activeIndex]) return;
              hideSuggestion(suggestions[activeIndex]);
              setStatus(t("composer.recipientHidden"));
            }}
          >
            {t("composer.hideSelectedRecipient")}
          </button>
        </div>
      ) : null}
      <span
        className="recipient-combobox-status"
        role="status"
        aria-live="polite"
      >
        {status}
      </span>
      {error ? (
        <p className="recipient-combobox-error" id={errorId} role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}

function formatSuggestion(suggestion: ContactedPersonSuggestion) {
  if (suggestion.formatted_address?.trim())
    return suggestion.formatted_address.trim();
  const recipient: MailAddress = {
    address: suggestion.address,
    name: suggestion.display_name ?? undefined,
  };
  return formatAddress(recipient);
}
