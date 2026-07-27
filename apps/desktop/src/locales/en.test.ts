import { describe, expect, it } from "vitest";
import { en } from "./en";

describe("feedback status translations", () => {
  it("contains every status shown after unsubscribing", () => {
    expect(en.translation.feedback).toMatchObject({
      unsubscribeSuccess: "Unsubscribe request sent",
      unsubscribeWeb: "Opened the unsubscribe page",
      unsubscribeFailed: "Could not unsubscribe",
    });
  });

  it("keeps category-save feedback with the other app status messages", () => {
    expect(en.translation.feedback.categorySaved).toBe("Category saved");
  });
});
