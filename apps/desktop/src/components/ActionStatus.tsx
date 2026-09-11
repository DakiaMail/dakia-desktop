import { IconAlertCircle, IconCheck, IconX } from "@tabler/icons-react";

type Props = {
  message: string;
  tone?: "success" | "error";
  action?: {
    label: string;
    onAction: () => void;
    disabled?: boolean;
  };
  dismiss?: { label: string; onDismiss: () => void };
};

export function ActionStatus({
  message,
  tone = "success",
  action,
  dismiss,
}: Props) {
  return (
    <div
      className="action-status"
      data-tone={tone}
      role="status"
      aria-live="polite"
      aria-atomic="true"
    >
      {tone === "error" ? (
        <IconAlertCircle size={16} stroke={2} />
      ) : (
        <IconCheck size={16} stroke={2.2} />
      )}
      <span>{message}</span>
      {action ? (
        <button
          type="button"
          className="action-status-action"
          onClick={action.onAction}
          disabled={action.disabled}
        >
          {action.label}
        </button>
      ) : null}
      {dismiss ? (
        <button
          type="button"
          className="action-status-dismiss"
          onClick={dismiss.onDismiss}
          aria-label={dismiss.label}
        >
          <IconX size={16} stroke={2} />
        </button>
      ) : null}
    </div>
  );
}
