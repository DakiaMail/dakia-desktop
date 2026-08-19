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

  it("provides the irreversible delete confirmation and result copy", () => {
    expect(en.translation.actions.permanentlyDelete).toBe("Permanently delete");
    expect(en.translation.reader).toMatchObject({
      permanentDeleteTitle: "Permanently delete this message?",
      permanentDeleteBody:
        "Dakia will request deletion of this message without moving it to Trash. It cannot be undone in Dakia; your email provider controls the final IMAP disposition.",
    });
    expect(en.translation.feedback).toMatchObject({
      permanentDeleteSuccess: "Message deletion request completed",
      permanentDeleteFailed: "Could not permanently delete this message",
    });
    expect(en.translation.shortcuts.permanentlyDelete).toBe(
      "Permanently delete opened message",
    );
  });
});
