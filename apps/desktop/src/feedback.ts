import { getVersion } from "@tauri-apps/api/app";
import {
  arch,
  type as osType,
  version as osVersion,
} from "@tauri-apps/plugin-os";
import type { ComposeSeed } from "./composeWindow";

export const DAKIA_SUPPORT_ADDRESS = "support@dakiamail.com";
export const FEEDBACK_SUBJECT = "Dakia feedback";
export const UNAVAILABLE_DIAGNOSTIC = "Unavailable";

/**
 * Deliberately limited to diagnostics the user can review before sending.
 * Account and mailbox data do not belong in this type or in the feedback body.
 */
export type FeedbackEnvironment = {
  appVersion?: string;
  osName?: string;
  osVersion?: string;
  architecture?: string;
  locale?: string;
};

async function readDiagnostic(
  diagnostic: () =>
    string | null | undefined | Promise<string | null | undefined>,
): Promise<string | undefined> {
  try {
    return (await diagnostic()) || undefined;
  } catch {
    return undefined;
  }
}

/**
 * Loads each diagnostic independently. Native APIs may be unavailable in the
 * browser preview or fail individually, neither of which should block feedback.
 */
export async function loadFeedbackEnvironment(
  locale: string | undefined,
): Promise<FeedbackEnvironment> {
  const [appVersion, osName, detectedOsVersion, architecture] =
    await Promise.all([
      readDiagnostic(getVersion),
      readDiagnostic(osType),
      readDiagnostic(osVersion),
      readDiagnostic(arch),
    ]);

  return {
    appVersion,
    osName,
    osVersion: detectedOsVersion,
    architecture,
    locale: locale || undefined,
  };
}

function displayDiagnostic(value: string | undefined): string {
  return value || UNAVAILABLE_DIAGNOSTIC;
}

export function createFeedbackBody(environment: FeedbackEnvironment): string {
  return [
    "Hi Dakia team,",
    "",
    "[Write your feedback here]",
    "",
    "---",
    "Automatically included:",
    `Dakia version: ${displayDiagnostic(environment.appVersion)}`,
    `Operating system: ${displayDiagnostic(environment.osName)} ${displayDiagnostic(environment.osVersion)}`,
    `Architecture: ${displayDiagnostic(environment.architecture)}`,
    `Language: ${displayDiagnostic(environment.locale)}`,
  ].join("\n");
}

/**
 * Creates an editable, privacy-conscious message seed. This promise always
 * resolves so unavailable diagnostics cannot prevent opening the composer.
 */
export async function createFeedbackComposeSeed(
  accountId: string | undefined,
  locale: string,
): Promise<ComposeSeed> {
  const environment = await loadFeedbackEnvironment(locale);
  return {
    accountId,
    to: DAKIA_SUPPORT_ADDRESS,
    subject: FEEDBACK_SUBJECT,
    body: createFeedbackBody(environment),
  };
}
