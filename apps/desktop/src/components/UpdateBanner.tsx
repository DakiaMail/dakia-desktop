import { useTranslation } from "react-i18next";
import type { AvailableUpdate, DownloadProgress } from "../updater";

export type UpdateBannerState =
  | { phase: "available"; update: AvailableUpdate }
  | {
      phase: "downloading";
      update: AvailableUpdate;
      progress: DownloadProgress;
    }
  | { phase: "ready"; update: AvailableUpdate }
  | { phase: "installing"; update: AvailableUpdate }
  | {
      phase: "error";
      update: AvailableUpdate;
      operation: "download" | "install";
      message: string;
    };

type Props = {
  state: UpdateBannerState;
  onDownload: () => void;
  onInstall: () => void;
  onDismiss: () => void;
};

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function UpdateBanner({
  state,
  onDownload,
  onInstall,
  onDismiss,
}: Props) {
  const { t } = useTranslation();
  const { update } = state;
  const percentage =
    state.phase === "downloading" && state.progress.totalBytes
      ? Math.min(
          100,
          Math.round(
            (state.progress.downloadedBytes / state.progress.totalBytes) * 100,
          ),
        )
      : undefined;

  let detail = update.notes;
  if (state.phase === "downloading") {
    detail =
      percentage === undefined
        ? t("updates.downloadingBytes", {
            downloaded: formatBytes(state.progress.downloadedBytes),
          })
        : t("updates.downloadingProgress", { percentage });
  } else if (state.phase === "ready") {
    detail = t("updates.ready");
  } else if (state.phase === "installing") {
    detail = t("updates.installing");
  } else if (state.phase === "error") {
    detail =
      state.operation === "download"
        ? t("updates.downloadFailed", { error: state.message })
        : t("updates.installFailed", { error: state.message });
  }

  return (
    <aside className="update-banner" aria-live="polite">
      <div className="update-banner-copy">
        <strong>{t("updates.available", { version: update.version })}</strong>
        {detail ? <span className="update-banner-notes">{detail}</span> : null}
        {state.phase === "downloading" ? (
          <progress
            aria-label={t("updates.downloadProgressLabel")}
            max={state.progress.totalBytes}
            value={
              state.progress.totalBytes
                ? state.progress.downloadedBytes
                : undefined
            }
          />
        ) : null}
      </div>
      <div className="update-banner-actions">
        {state.phase === "available" ||
        (state.phase === "error" && state.operation === "download") ? (
          <button className="update-banner-primary" onClick={onDownload}>
            {state.phase === "error"
              ? t("updates.retry")
              : t("updates.download")}
          </button>
        ) : null}
        {state.phase === "ready" ||
        (state.phase === "error" && state.operation === "install") ? (
          <button className="update-banner-primary" onClick={onInstall}>
            {state.phase === "error"
              ? t("updates.retry")
              : t("updates.installAndRestart")}
          </button>
        ) : null}
        {state.phase !== "downloading" && state.phase !== "installing" ? (
          <button className="update-banner-secondary" onClick={onDismiss}>
            {t("updates.later")}
          </button>
        ) : null}
      </div>
    </aside>
  );
}
