import React from "react";
import {
  MantineProvider,
  createTheme,
  localStorageColorSchemeManager,
} from "@mantine/core";
import "@mantine/core/styles.css";
import "./i18n";
import "./styles.css";
import App from "./App";
import { ComposeApp } from "./ComposeApp";
import { ReaderWindowApp } from "./ReaderWindowApp";
import { AccountWindowApp, SettingsWindowApp } from "./UtilityApps";

const theme = createTheme({
  primaryColor: "ember",
  fontFamily: '"Inter Variable", Inter, sans-serif',
  fontFamilyMonospace: '"SFMono-Regular", "Cascadia Code", monospace',
  headings: {
    fontFamily: '"Manrope Variable", Manrope, sans-serif',
    fontWeight: "650",
  },
  colors: {
    ember: [
      "#fff3ef",
      "#f8e1d8",
      "#f0c0ae",
      "#e79c80",
      "#df7d5d",
      "#d96945",
      "#d65a3a",
      "#bd4930",
      "#a83d28",
      "#94331f",
    ],
  },
  defaultRadius: "md",
  cursorType: "pointer",
});
const colorSchemeManager = localStorageColorSchemeManager({
  key: "dakia.color-scheme",
});

export function DesktopRoot() {
  const view = new URLSearchParams(window.location.search).get("view");
  const content =
    view === "compose" ? (
      <ComposeApp />
    ) : view === "reader" ? (
      <ReaderWindowApp />
    ) : view === "settings" ? (
      <SettingsWindowApp />
    ) : view === "account" ? (
      <AccountWindowApp />
    ) : (
      <App />
    );
  return (
    <React.StrictMode>
      <MantineProvider
        theme={theme}
        colorSchemeManager={colorSchemeManager}
        defaultColorScheme="auto"
      >
        {content}
      </MantineProvider>
    </React.StrictMode>
  );
}
