import { describe, expect, it } from "vitest";
import {
  compareMailTimestamps,
  formatMailTimestamp,
  localMailTimestamp,
  mailTimestampForDisplay,
} from "./mailTimestamp";

describe("mailTimestampForDisplay", () => {
  it("preserves ordinary RFC 3339 instants and offsets", () => {
    expect(
      mailTimestampForDisplay("2024-02-29T12:34:56.789+05:30")?.toISOString(),
    ).toBe("2024-02-29T07:04:56.789Z");
    expect(
      mailTimestampForDisplay("3001-01-01T00:00:00+00:00")?.toISOString(),
    ).toBe("3001-01-01T00:00:00.000Z");
  });

  it("maps a valid leap second to the final representable millisecond", () => {
    expect(
      mailTimestampForDisplay("2016-12-31T23:59:60+00:00")?.toISOString(),
    ).toBe("2016-12-31T23:59:59.999Z");
    expect(
      mailTimestampForDisplay("2017-01-01T00:59:60+01:00")?.toISOString(),
    ).toBe("2016-12-31T23:59:59.999Z");
    expect(
      mailTimestampForDisplay("2026-03-31T23:59:60+00:00")?.toISOString(),
    ).toBe("2026-03-31T23:59:59.999Z");
  });

  it("orders a leap second after the ordinary final millisecond", () => {
    const ordinary = "2016-12-31T23:59:59.999+00:00";
    const leap = "2016-12-31T23:59:60+00:00";
    const fractionalLeap = "2016-12-31T23:59:60.5+00:00";
    const followingMinute = "2017-01-01T00:00:00+00:00";

    expect(compareMailTimestamps(ordinary, leap)).toBeLessThan(0);
    expect(compareMailTimestamps(leap, fractionalLeap)).toBeLessThan(0);
    expect(compareMailTimestamps(fractionalLeap, followingMinute)).toBeLessThan(
      0,
    );
    expect(compareMailTimestamps("not a date", ordinary)).toBeLessThan(0);
    expect(compareMailTimestamps("not a date", "also invalid")).toBe(0);
  });

  it("rejects malformed values instead of normalizing them into another date", () => {
    for (const value of [
      "2016-02-30T23:59:60+00:00",
      "2016-12-31T23:59:61+00:00",
      "2016-12-31T23:59:60",
      "2016-12-31 23:59:60+00:00",
      "2016-12-31T23:59:59+24:00",
      "2026-04-10T12:34:60Z",
    ]) {
      expect(mailTimestampForDisplay(value)).toBeUndefined();
    }
  });

  it("provides a bounded label for missing or malformed timestamps", () => {
    expect(formatMailTimestamp(undefined, "PP")).toBe("Unknown date");
    expect(formatMailTimestamp("not a date", "PP")).toBe("Unknown date");
    expect(localMailTimestamp("not a date")).toBe("Unknown date");
  });
});
