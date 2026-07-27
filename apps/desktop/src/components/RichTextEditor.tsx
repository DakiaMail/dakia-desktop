import {
  IconBold,
  IconClearFormatting,
  IconCode,
  IconItalic,
  IconLink,
  IconList,
  IconListNumbers,
  IconQuote,
  IconStrikethrough,
  IconUnderline,
} from "@tabler/icons-react";
import {
  useEffect,
  useRef,
  useState,
  type ClipboardEvent,
  type ReactNode,
} from "react";
import { useTranslation } from "react-i18next";
import { sanitizeRichText } from "./richText";

type Props = {
  value: string;
  disabled?: boolean;
  onChange: (html: string) => void;
};

type Command =
  | "bold"
  | "italic"
  | "underline"
  | "strikeThrough"
  | "insertUnorderedList"
  | "insertOrderedList"
  | "formatBlock"
  | "removeFormat";

export function RichTextEditor({ value, disabled = false, onChange }: Props) {
  const { t } = useTranslation();
  const editorRef = useRef<HTMLDivElement>(null);
  const latestValue = useRef<string | null>(null);
  const selectionRef = useRef<Range | null>(null);
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");

  useEffect(() => {
    if (value === latestValue.current) return;
    const editor = editorRef.current;
    if (editor) editor.innerHTML = value;
    latestValue.current = value;
  }, [value]);

  const publish = () => {
    const editor = editorRef.current;
    if (!editor) return;
    const html = sanitizeRichText(editor.innerHTML);
    if (html !== editor.innerHTML) editor.innerHTML = html;
    latestValue.current = html;
    onChange(html);
  };

  const run = (command: Command, argument?: string) => {
    if (disabled) return;
    editorRef.current?.focus();
    document.execCommand(command, false, argument);
    publish();
  };

  const rememberSelection = () => {
    const selection = window.getSelection();
    if (selection?.rangeCount) {
      selectionRef.current = selection.getRangeAt(0).cloneRange();
    }
  };

  const restoreSelection = () => {
    if (!selectionRef.current) return;
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(selectionRef.current);
  };

  const addLink = () => {
    const href = linkUrl.trim();
    if (!href || disabled) return;
    editorRef.current?.focus();
    restoreSelection();
    document.execCommand("createLink", false, href);
    publish();
    setLinkUrl("");
    setLinkOpen(false);
  };

  const paste = (event: ClipboardEvent<HTMLDivElement>) => {
    event.preventDefault();
    const clipboard = event.clipboardData;
    const html = clipboard.getData("text/html");
    const text = clipboard.getData("text/plain");
    const content = html ? sanitizeRichText(html) : text.replace(/\n/g, "<br>");
    document.execCommand("insertHTML", false, content);
    publish();
  };

  const toolbarButton = (
    command: Command,
    label: string,
    icon: ReactNode,
    argument?: string,
  ) => (
    <button
      className="compose-format-button"
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onMouseDown={(event) => event.preventDefault()}
      onClick={() => run(command, argument)}
    >
      {icon}
    </button>
  );

  return (
    <section
      className="compose-rich-editor"
      aria-label={t("composer.formatting")}
    >
      <div
        ref={editorRef}
        className="compose-editor"
        role="textbox"
        aria-multiline="true"
        aria-label={t("composer.body")}
        aria-placeholder={t("composer.body")}
        contentEditable={!disabled}
        suppressContentEditableWarning
        data-placeholder={t("composer.body")}
        aria-disabled={disabled || undefined}
        onInput={publish}
        onPaste={paste}
        onDrop={(event) => event.preventDefault()}
        onDragOver={(event) => event.preventDefault()}
      />
      <div
        className="compose-format-toolbar"
        role="toolbar"
        aria-label={t("composer.formatting")}
      >
        {toolbarButton(
          "bold",
          t("composer.format.bold"),
          <IconBold size={16} />,
          undefined,
        )}
        {toolbarButton(
          "italic",
          t("composer.format.italic"),
          <IconItalic size={16} />,
          undefined,
        )}
        {toolbarButton(
          "underline",
          t("composer.format.underline"),
          <IconUnderline size={16} />,
          undefined,
        )}
        {toolbarButton(
          "strikeThrough",
          t("composer.format.strikethrough"),
          <IconStrikethrough size={16} />,
          undefined,
        )}
        <span className="compose-format-separator" aria-hidden="true" />
        {toolbarButton(
          "formatBlock",
          t("composer.format.quote"),
          <IconQuote size={16} />,
          "blockquote",
        )}
        {toolbarButton(
          "formatBlock",
          t("composer.format.code"),
          <IconCode size={16} />,
          "pre",
        )}
        {toolbarButton(
          "insertUnorderedList",
          t("composer.format.bulletedList"),
          <IconList size={16} />,
          undefined,
        )}
        {toolbarButton(
          "insertOrderedList",
          t("composer.format.numberedList"),
          <IconListNumbers size={16} />,
          undefined,
        )}
        <span className="compose-format-separator" aria-hidden="true" />
        <button
          className="compose-format-button"
          type="button"
          aria-label={t("composer.format.link")}
          title={t("composer.format.link")}
          disabled={disabled}
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => {
            rememberSelection();
            setLinkOpen((open) => !open);
          }}
        >
          <IconLink size={16} />
        </button>
        {toolbarButton(
          "removeFormat",
          t("composer.format.clear"),
          <IconClearFormatting size={16} />,
          undefined,
        )}
        {linkOpen ? (
          <form
            className="compose-link-form"
            onSubmit={(event) => {
              event.preventDefault();
              addLink();
            }}
          >
            <label className="sr-only" htmlFor="compose-link-url">
              {t("composer.format.linkUrl")}
            </label>
            <input
              id="compose-link-url"
              value={linkUrl}
              onChange={(event) => setLinkUrl(event.currentTarget.value)}
              placeholder={t("composer.format.linkUrl")}
              inputMode="url"
              autoFocus
              disabled={disabled}
            />
            <button type="submit" disabled={!linkUrl.trim() || disabled}>
              {t("composer.format.applyLink")}
            </button>
          </form>
        ) : null}
      </div>
    </section>
  );
}
