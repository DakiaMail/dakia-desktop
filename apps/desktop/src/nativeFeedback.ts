import { ask, message } from "@tauri-apps/plugin-dialog";

type FeedbackKind = "info" | "warning" | "error";

const isTauri = () => "__TAURI_INTERNALS__" in window;

export async function showNativeMessage(
  title: string,
  detail: string,
  kind: FeedbackKind = "info",
) {
  if (isTauri()) {
    await message(detail, { title, kind });
    return;
  }

  const write = kind === "error" ? console.error : console.info;
  write(`${title}: ${detail}`);
}

export async function confirmNativeAction(
  title: string,
  detail: string,
  okLabel: string,
) {
  if (isTauri()) {
    return ask(detail, {
      title,
      kind: "warning",
      okLabel,
      cancelLabel: "Cancel",
    });
  }
  return window.confirm(`${title}\n\n${detail}`);
}
