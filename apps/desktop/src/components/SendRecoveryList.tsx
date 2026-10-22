import { useTranslation } from "react-i18next";
import type { SendRecoverySummary } from "../types";

type Props = {
  items: SendRecoverySummary[];
  accountNames?: Record<string, string>;
  loading: boolean;
  failed: boolean;
  hasMore: boolean;
  onOpen: (item: SendRecoverySummary) => void;
  onRetry: () => void;
  onLoadMore: () => void;
};

export function SendRecoveryList({
  items,
  accountNames = {},
  loading,
  failed,
  hasMore,
  onOpen,
  onRetry,
  onLoadMore,
}: Props) {
  const { t } = useTranslation();
  if (!items.length && !loading && !failed && !hasMore) return null;
  return (
    <section className="send-recoveries" aria-label={t("sendRecovery.title")}>
      <h2>{t("sendRecovery.title")}</h2>
      <ul>
        {items.map((item) => (
          <li key={`${item.account_id}:${item.attempt_id}`}>
            <button
              className="send-recovery-open"
              onClick={() => onOpen(item)}
              aria-label={`${t("sendRecovery.open")}: ${item.subject || t("sendRecovery.noSubject")}`}
            >
              {accountNames[item.account_id] ? (
                <span>{accountNames[item.account_id]}</span>
              ) : null}
              <span>{item.subject || t("sendRecovery.noSubject")}</span>
              <span className="send-recovery-outcome">
                {t(
                  item.status === "not_submitted"
                    ? "sendRecovery.notSubmitted"
                    : item.status === "sent_copy_failed"
                      ? "sendRecovery.sentCopyFailed"
                      : "sendRecovery.deliveryUncertain",
                )}
              </span>
            </button>
          </li>
        ))}
      </ul>
      {loading ? <p role="status">{t("sendRecovery.loading")}</p> : null}
      {failed ? (
        <div role="alert">
          <p>{t("sendRecovery.loadError")}</p>
          <button onClick={onRetry} disabled={loading}>
            {t("sendRecovery.retry")}
          </button>
        </div>
      ) : null}
      {hasMore && !failed ? (
        <button onClick={onLoadMore} disabled={loading}>
          {t("sendRecovery.loadMore")}
        </button>
      ) : null}
    </section>
  );
}
