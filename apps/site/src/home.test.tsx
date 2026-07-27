import { describe, expect, it } from "vitest";
import { offlineTranslationStory, openSourceStory } from "./home";
import { translationSceneCopy } from "./scenes";

describe("public offline translation story", () => {
  it("prominently presents private offline translation and keeps AI claims separate", () => {
    expect(offlineTranslationStory.title).toBe(
      "Translate the whole email. Send nothing to the cloud.",
    );
    expect(offlineTranslationStory.description).toContain(
      "Download a verified language pack once",
    );
    expect(offlineTranslationStory.description).toContain("translate offline");
    expect(offlineTranslationStory.bullets).toContain(
      "Email content never leaves your device",
    );
    expect(offlineTranslationStory.bullets).toContain(
      "Preserves the original message layout",
    );
  });

  it("demonstrates translating and restoring the original message", () => {
    expect(translationSceneCopy.action).toBe("Translate to English");
    expect(translationSceneCopy.restoreAction).toBe("Show original");
    expect(translationSceneCopy.original.subject).toBe("Reedese töötoa plaan");
    expect(translationSceneCopy.translated.subject).toBe(
      "Friday’s workshop plan",
    );
    expect(translationSceneCopy.status).toBe(
      "Translated from Estonian on this device",
    );
  });
});

describe("public open-source story", () => {
  it("states that Dakia is open source and invites public inspection", () => {
    expect(openSourceStory.hero).toContain("open-source desktop app");
    expect(openSourceStory.title).toContain("open by design");
    expect(openSourceStory.description).toContain("source code is public");
    expect(openSourceStory.description).toContain(
      "inspect how it handles mail",
    );
  });
});
