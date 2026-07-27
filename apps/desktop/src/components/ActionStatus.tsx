import { IconAlertCircle, IconCheck } from "@tabler/icons-react";

type Props = {
  message: string;
  tone?: "success" | "error";
};

export function ActionStatus({ message, tone = "success" }: Props) {
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
    </div>
  );
}
