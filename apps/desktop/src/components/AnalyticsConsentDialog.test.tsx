import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { AnalyticsConsentDialog } from "./AnalyticsConsentDialog";

const analyticsMocks = vi.hoisted(() => ({
  createAnalyticsPayload: vi.fn(async () => ({
    schema: 1,
    month: "2026-08",
    app_version: "0.4.0",
    os: "macos",
    os_version: "15.6",
    arch: "arm64",
    providers: ["fastmail"],
  })),
}));

vi.mock("../analytics", () => ({
  createAnalyticsPayload: analyticsMocks.createAnalyticsPayload,
}));

describe("AnalyticsConsentDialog", () => {
  beforeEach(() => vi.clearAllMocks());

  it("requires an explicit first-run choice and keeps the payload preview closed", () => {
    const onChoose = vi.fn();
    render(
      <MantineProvider>
        <AnalyticsConsentDialog accounts={[]} opened onChoose={onChoose} />
      </MantineProvider>,
    );

    expect(screen.getByText("Help shape Dakia")).toBeVisible();
    expect(
      screen.getByText("See exactly what Dakia sends").closest("details"),
    ).not.toHaveAttribute("open");
    expect(analyticsMocks.createAnalyticsPayload).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", { name: "Keep statistics off" }),
    );
    expect(onChoose).toHaveBeenCalledWith(false);
  });

  it("records an affirmative choice only when the user selects sharing", () => {
    const onChoose = vi.fn();
    render(
      <MantineProvider>
        <AnalyticsConsentDialog accounts={[]} opened onChoose={onChoose} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Share anonymous statistics" }),
    );
    expect(onChoose).toHaveBeenCalledWith(true);
  });

  it("loads the live payload only after expanding its disclosure", async () => {
    render(
      <MantineProvider>
        <AnalyticsConsentDialog accounts={[]} opened onChoose={vi.fn()} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByText("See exactly what Dakia sends"));
    await waitFor(() =>
      expect(analyticsMocks.createAnalyticsPayload).toHaveBeenCalledWith(
        [],
        false,
      ),
    );
  });
});
