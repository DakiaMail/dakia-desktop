import { format } from "date-fns";

export const UNKNOWN_MAIL_DATE = "Unknown date";

const rfc3339Timestamp =
  /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(\.\d+)?(Z|[+-]\d{2}:\d{2})$/;

type ParsedMailTimestamp = {
  readonly display: Date;
  readonly leapSecond: boolean;
  readonly fraction: string;
};

function parsedMilliseconds(fraction: string | undefined) {
  if (!fraction) return 0;
  return Number(fraction.slice(1, 4).padEnd(3, "0"));
}

function hasUtcFields(
  value: Date,
  fields: {
    year: number;
    month: number;
    day: number;
    hour: number;
    minute: number;
    second: number;
    millisecond: number;
  },
) {
  return (
    value.getUTCFullYear() === fields.year &&
    value.getUTCMonth() === fields.month - 1 &&
    value.getUTCDate() === fields.day &&
    value.getUTCHours() === fields.hour &&
    value.getUTCMinutes() === fields.minute &&
    value.getUTCSeconds() === fields.second &&
    value.getUTCMilliseconds() === fields.millisecond
  );
}

function isLeapSecondPosition(value: Date) {
  const month = value.getUTCMonth();
  const lastDay = new Date(value.getTime());
  lastDay.setUTCDate(1);
  lastDay.setUTCMonth(month + 1);
  lastDay.setUTCDate(0);
  return (
    value.getUTCDate() === lastDay.getUTCDate() &&
    value.getUTCHours() === 23 &&
    value.getUTCMinutes() === 59 &&
    value.getUTCSeconds() === 59
  );
}

function compareFractions(left: string, right: string) {
  const length = Math.max(left.length, right.length);
  const leftValue = left.padEnd(length, "0");
  const rightValue = right.padEnd(length, "0");
  if (leftValue < rightValue) return -1;
  if (leftValue > rightValue) return 1;
  return 0;
}

/**
 * Converts a strictly valid RFC 3339 mail timestamp into a JavaScript Date
 * for display only. JavaScript cannot represent RFC 3339's `:60` leap second,
 * so a valid one is shown at the final representable millisecond of its minute.
 */
function parseMailTimestamp(
  value: string | null | undefined,
): ParsedMailTimestamp | undefined {
  if (typeof value !== "string") return undefined;
  const match = rfc3339Timestamp.exec(value);
  if (!match) return undefined;

  const [
    ,
    yearText,
    monthText,
    dayText,
    hourText,
    minuteText,
    secondText,
    fraction,
    zone,
  ] = match;
  const year = Number(yearText);
  const month = Number(monthText);
  const day = Number(dayText);
  const hour = Number(hourText);
  const minute = Number(minuteText);
  const second = Number(secondText);
  const leapSecond = second === 60;
  if (
    month < 1 ||
    month > 12 ||
    day < 1 ||
    hour > 23 ||
    minute > 59 ||
    second > 60
  ) {
    return undefined;
  }

  const offsetHour = zone === "Z" ? 0 : Number(zone.slice(1, 3));
  const offsetMinute = zone === "Z" ? 0 : Number(zone.slice(4, 6));
  if (offsetHour > 23 || offsetMinute > 59) return undefined;
  const offset =
    (offsetHour * 60 + offsetMinute) * 60_000 * (zone.startsWith("-") ? -1 : 1);
  const fields = {
    year,
    month,
    day,
    hour,
    minute,
    second: leapSecond ? 59 : second,
    millisecond: leapSecond ? 999 : parsedMilliseconds(fraction),
  };

  const local = new Date(0);
  local.setUTCFullYear(fields.year, fields.month - 1, fields.day);
  local.setUTCHours(
    fields.hour,
    fields.minute,
    fields.second,
    fields.millisecond,
  );
  if (!Number.isFinite(local.getTime()) || !hasUtcFields(local, fields)) {
    return undefined;
  }

  const instant = new Date(local.getTime() - offset);
  const roundTrip = new Date(instant.getTime() + offset);
  if (
    !Number.isFinite(instant.getTime()) ||
    !hasUtcFields(roundTrip, fields) ||
    (leapSecond && !isLeapSecondPosition(instant))
  ) {
    return undefined;
  }
  return {
    display: instant,
    leapSecond,
    fraction: fraction?.slice(1) ?? "",
  };
}

export function mailTimestampForDisplay(
  value: string | null | undefined,
): Date | undefined {
  return parseMailTimestamp(value)?.display;
}

/**
 * Orders valid mail timestamps without treating a leap second as equal to the
 * ordinary final millisecond used for its JavaScript display representation.
 * Invalid values stay in the existing caller-owned deterministic tie bucket.
 */
export function compareMailTimestamps(
  left: string | null | undefined,
  right: string | null | undefined,
) {
  const leftTimestamp = parseMailTimestamp(left);
  const rightTimestamp = parseMailTimestamp(right);
  if (!leftTimestamp && !rightTimestamp) return 0;
  if (!leftTimestamp) return -1;
  if (!rightTimestamp) return 1;

  const milliseconds =
    leftTimestamp.display.getTime() - rightTimestamp.display.getTime();
  if (milliseconds) return milliseconds;
  const leapSecond =
    Number(leftTimestamp.leapSecond) - Number(rightTimestamp.leapSecond);
  return (
    leapSecond ||
    compareFractions(leftTimestamp.fraction, rightTimestamp.fraction)
  );
}

export function formatMailTimestamp(
  value: string | null | undefined,
  pattern: string,
) {
  const timestamp = mailTimestampForDisplay(value);
  return timestamp ? format(timestamp, pattern) : UNKNOWN_MAIL_DATE;
}

export function localMailTimestamp(value: string | null | undefined) {
  return mailTimestampForDisplay(value)?.toLocaleString() ?? UNKNOWN_MAIL_DATE;
}
