import { Code, Text } from "@mantine/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { createAnalyticsPayload, type AnalyticsPayload } from "../analytics";
import type { Account } from "../types";

type Props = {
  accounts: Account[];
  enabled: boolean;
};

/** A native disclosure keeps the exact outbound data hidden until requested. */
export function AnalyticsDataPreview({ accounts, enabled }: Props) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [payload, setPayload] = useState<AnalyticsPayload>();

  useEffect(() => {
    if (!expanded) {
      setPayload(undefined);
      return;
    }
    let current = true;
    void createAnalyticsPayload(accounts, enabled).then((next) => {
      if (current) setPayload(next);
    });
    return () => {
      current = false;
    };
  }, [accounts, enabled, expanded]);

  return (
    <details
      className="analytics-data-preview"
      onToggle={(event) => setExpanded(event.currentTarget.open)}
    >
      <summary>{t("settings.analyticsPreview")}</summary>
      <Text size="xs" c="dimmed" mt="xs">
        {enabled
          ? t("settings.analyticsPreviewEnabled")
          : t("settings.analyticsPreviewDisabled")}
      </Text>
      {payload ? (
        <Code block mt="xs" aria-label={t("settings.analyticsPreview")}>
          {JSON.stringify(payload, null, 2)}
        </Code>
      ) : (
        <Text size="sm" mt="xs">
          {t("settings.analyticsPreviewLoading")}
        </Text>
      )}
    </details>
  );
}
