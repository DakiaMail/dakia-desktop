import { describe, expect, it } from "vitest";
import { outboxItemId } from "./outboxIdentity";

describe("account-scoped optimistic submission identity", () => {
  it("keeps an equal attempt UUID in two accounts distinct", () => {
    const attempt = "30000000-0000-4000-8000-000000000003";
    expect(outboxItemId("10000000-0000-4000-8000-000000000001", attempt)).toBe(
      "outbox-10000000-0000-4000-8000-000000000001-30000000-0000-4000-8000-000000000003",
    );
    expect(outboxItemId("20000000-0000-4000-8000-000000000002", attempt)).toBe(
      "outbox-20000000-0000-4000-8000-000000000002-30000000-0000-4000-8000-000000000003",
    );
  });
});
