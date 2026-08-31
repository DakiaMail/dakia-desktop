import { Anchor, Button, Group, Modal, Stack, Text } from "@mantine/core";
import { useTranslation } from "react-i18next";
import { ANALYTICS_PRIVACY_URL } from "../analytics";
import { api } from "../api";
import type { Account } from "../types";
import { AnalyticsDataPreview } from "./AnalyticsDataPreview";

type Props = {
  accounts: Account[];
  opened: boolean;
  onChoose: (enabled: boolean) => void;
};

export function AnalyticsConsentDialog({ accounts, opened, onChoose }: Props) {
  const { t } = useTranslation();
  return (
    <Modal
      opened={opened}
      onClose={() => onChoose(false)}
      closeOnClickOutside={false}
      closeOnEscape={false}
      withCloseButton={false}
      title={t("settings.analyticsTitle")}
      centered
    >
      <Stack gap="md">
        <Text>{t("settings.analyticsBody")}</Text>
        <Text size="sm" c="dimmed">
          {t("settings.analyticsDisclosure")}
        </Text>
        <Text size="sm" c="dimmed">
          {t("settings.analyticsProcessingDisclosure")}{" "}
          <Anchor
            component="button"
            type="button"
            onClick={() => void api.openExternal(ANALYTICS_PRIVACY_URL)}
          >
            {t("settings.analyticsPrivacyDetails")}
          </Anchor>
        </Text>
        <AnalyticsDataPreview accounts={accounts} enabled={false} />
        <Group justify="flex-end">
          <Button variant="default" onClick={() => onChoose(false)}>
            {t("settings.analyticsKeepOff")}
          </Button>
          <Button onClick={() => onChoose(true)}>
            {t("settings.analyticsEnable")}
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}
