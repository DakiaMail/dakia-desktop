import {
  IconChevronDown,
  IconCirclePlus,
  IconArchive,
  IconEdit,
  IconInbox,
  IconMail,
  IconMailbox,
  IconMessageCircle,
  IconSend,
  IconShieldX,
  IconSparkles,
  IconStar,
  IconTrash,
} from "@tabler/icons-react";
import { useTranslation } from "react-i18next";
import { useState } from "react";
import type { Account } from "../types";

type Props = {
  accounts: Account[];
  selectedAccountId?: string;
  mailbox: string;
  onSelectAccount: (id: string) => void;
  onAccountContextMenu: (account: Account) => void;
  onAddAccount: () => void;
  onMailbox: (mailbox: string) => void;
  onFeedback: () => void;
  feedbackDisabled?: boolean;
  outboxCount?: number;
  starredCount?: number;
};

export function MailboxNav({
  accounts,
  selectedAccountId,
  mailbox,
  onSelectAccount,
  onAccountContextMenu,
  onAddAccount,
  onMailbox,
  onFeedback,
  feedbackDisabled = false,
  outboxCount = 0,
  starredCount = 0,
}: Props) {
  const { t } = useTranslation();
  const [accountsExpanded, setAccountsExpanded] = useState(true);
  const links = [
    ["", t("nav.allMail"), IconMail],
    ["unread", t("nav.unread"), IconSparkles],
    ["starred", t("nav.starred"), IconStar],
    ["Outbox", t("nav.outbox"), IconMailbox],
    ["Sent", t("nav.sent"), IconSend],
    ["Drafts", t("nav.drafts"), IconEdit],
    ["Archive", t("nav.archive"), IconArchive],
    ["Spam", t("nav.spam"), IconShieldX],
    ["Trash", t("nav.trash"), IconTrash],
  ] as const;
  return (
    <nav className="mailbox-nav" aria-label={t("app.name")}>
      <div className="mailbox-nav-scroll">
        <div className="nav-group-label">{t("nav.mailboxes")}</div>
        <div
          className="inbox-accordion-row"
          data-active={mailbox === "INBOX" && !selectedAccountId}
        >
          <button
            className="nav-link nav-link-primary inbox-nav-main"
            data-active={mailbox === "INBOX" && !selectedAccountId}
            onClick={() => onMailbox("INBOX")}
          >
            <IconInbox size={19} stroke={1.8} />
            <span>{t("nav.inbox")}</span>
          </button>
          <button
            className="inbox-toggle"
            onClick={() => setAccountsExpanded((value) => !value)}
            aria-expanded={accountsExpanded}
            aria-controls="inbox-account-list"
            aria-label={
              accountsExpanded
                ? t("actions.collapseAccounts")
                : t("actions.expandAccounts")
            }
          >
            <IconChevronDown
              className="inbox-chevron"
              data-expanded={accountsExpanded}
              size={15}
            />
          </button>
        </div>
        {accountsExpanded ? (
          <div
            id="inbox-account-list"
            className="account-list"
            aria-label={t("nav.accounts")}
          >
            {accounts.map((account, index) => {
              const active = selectedAccountId === account.id;
              return (
                <button
                  key={account.id}
                  className="account-row"
                  data-active={active}
                  onClick={() => onSelectAccount(account.id)}
                  onContextMenu={(event) => {
                    event.preventDefault();
                    onAccountContextMenu(account);
                  }}
                  aria-current={active ? "page" : undefined}
                  title={account.email}
                >
                  <span
                    className={`account-marker account-marker-${index % 5}`}
                  />
                  <span>{account.account_name}</span>
                </button>
              );
            })}
            <button className="account-row account-add" onClick={onAddAccount}>
              <IconCirclePlus size={15} stroke={1.7} />
              <span>{t("actions.addAccount")}</span>
            </button>
          </div>
        ) : null}
        <div className="nav-group-label nav-folders-label">
          {t("nav.folders")}
        </div>
        {links.map(([id, label, Icon]) => (
          <button
            key={id}
            className="nav-link"
            data-active={mailbox === id}
            onClick={() => onMailbox(id)}
          >
            <Icon size={18} stroke={1.7} />
            <span>{label}</span>
            {id === "Outbox" && outboxCount > 0 ? (
              <span
                className="nav-count"
                aria-label={t("nav.outboxCount", { count: outboxCount })}
              >
                {outboxCount}
              </span>
            ) : null}
            {id === "starred" && starredCount > 0 ? (
              <span
                className="nav-count"
                aria-label={t("nav.starredCount", { count: starredCount })}
              >
                {starredCount}
              </span>
            ) : null}
          </button>
        ))}
      </div>
      <div className="mailbox-nav-footer">
        <button
          className="nav-link sidebar-feedback"
          onClick={onFeedback}
          disabled={feedbackDisabled}
        >
          <IconMessageCircle size={18} stroke={1.7} />
          <span>{t("nav.feedback")}</span>
        </button>
      </div>
    </nav>
  );
}
