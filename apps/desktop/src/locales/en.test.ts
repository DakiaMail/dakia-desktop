import { describe, expect, it } from "vitest";
import { en } from "./en";

describe("feedback status translations", () => {
  it("describes terminal Outbox recoveries without calling them sending", () => {
    expect(en.translation.nav).toMatchObject({
      outboxSendingCount_one: "{{count}} email sending",
      outboxSendingCount_other: "{{count}} emails sending",
      outboxRecoveryCount_one: "{{count}} email needs attention",
      outboxRecoveryCount_other: "{{count}} emails need attention",
    });
  });

  it("explains terminal delivery outcomes without encouraging a duplicate send", () => {
    expect(en.translation.composer).toMatchObject({
      deliveryUncertain: "Delivery status unknown",
      sentCopyFailed: "Message sent, but saving it to Sent failed",
      deliveryUncertainDetail:
        "Dakia did not receive confirmation from the mail server. Check Sent before sending again.",
      sentCopyFailedDetail:
        "The mail server accepted this message, but Dakia could not save a copy in Sent. Do not send it again.",
      aiContextUnavailableTitle: "Could not prepare AI draft",
      aiContextUnavailable:
        "Dakia needs the current version of this email before it can share it with AI.",
      draftSaveError:
        "Dakia could not save this draft. Keep this window open while you check storage.",
      deliveredDraftClearError:
        "Dakia could not remove this delivered draft. Keep this window open.",
    });
  });

  it("contains every status shown after unsubscribing", () => {
    expect(en.translation.feedback).toMatchObject({
      unsubscribeSuccess: "Unsubscribe request sent.",
      unsubscribeWeb: "Unsubscribe page opened.",
      unsubscribeFailed: "Could not unsubscribe",
      unsubscribeCleanupAction: "Move to Trash",
      unsubscribeCleanupNone: "No emails from this sender were found",
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
