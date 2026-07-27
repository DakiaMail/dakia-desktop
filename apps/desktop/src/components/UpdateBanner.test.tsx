import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "../i18n";
import { UpdateBanner } from "./UpdateBanner";

const update = { version: "0.3.0", notes: "A safer release" };

describe("UpdateBanner", () => {
  it("offers a non-blocking download and Later action", () => {
    const onDownload = vi.fn();
    render(
      <UpdateBanner
        state={{ phase: "available", update }}
        onDownload={onDownload}
        onInstall={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );

    expect(screen.getByText("Dakia 0.3.0 is available")).toBeVisible();
    expect(screen.getByText("A safer release")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Download" }));
    expect(onDownload).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name: "Later" })).toBeVisible();
  });

  it("shows determinate download progress", () => {
    render(
      <UpdateBanner
        state={{
          phase: "downloading",
          update,
          progress: { downloadedBytes: 25, totalBytes: 100 },
        }}
        onDownload={vi.fn()}
        onInstall={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );

    expect(screen.getByText("Downloading… 25%")).toBeVisible();
    expect(screen.getByRole("progressbar")).toHaveAttribute("value", "25");
  });

  it("requires an explicit install and restart after download", () => {
    const onInstall = vi.fn();
    render(
      <UpdateBanner
        state={{ phase: "ready", update }}
        onDownload={vi.fn()}
        onInstall={onInstall}
        onDismiss={vi.fn()}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Install and Restart" }),
    );
    expect(onInstall).toHaveBeenCalledOnce();
  });

  it("offers retry after a download error", () => {
    const onDownload = vi.fn();
    render(
      <UpdateBanner
        state={{
          phase: "error",
          operation: "download",
          update,
          message: "network unavailable",
        }}
        onDownload={onDownload}
        onInstall={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );

    expect(screen.getByText(/network unavailable/)).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(onDownload).toHaveBeenCalledOnce();
  });
});
