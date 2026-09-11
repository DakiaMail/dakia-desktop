import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ActionStatus } from "./ActionStatus";

describe("ActionStatus", () => {
  it("exposes an optional action and dismiss control", () => {
    const onAction = vi.fn();
    const onDismiss = vi.fn();
    render(
      <ActionStatus
        message="Unsubscribe request sent"
        action={{ label: "Move to Trash", onAction }}
        dismiss={{ label: "Dismiss", onDismiss }}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Move to Trash" }));
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));

    expect(onAction).toHaveBeenCalledOnce();
    expect(onDismiss).toHaveBeenCalledOnce();
  });
});
