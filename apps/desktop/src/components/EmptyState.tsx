import { Button } from "@mantine/core";
import { IconMailPlus } from "@tabler/icons-react";

type Props = {
  title: string;
  body: string;
  action?: string;
  onAction?: () => void;
};

export function EmptyState({ title, body, action, onAction }: Props) {
  return (
    <div className="empty-state">
      <div className="empty-state-inner">
        <div className="empty-symbol">
          <IconMailPlus size={34} stroke={1.7} />
        </div>
        <div className="empty-title">{title}</div>
        <div className="empty-body">{body}</div>
        {action && onAction ? (
          <Button onClick={onAction}>{action}</Button>
        ) : null}
      </div>
    </div>
  );
}
