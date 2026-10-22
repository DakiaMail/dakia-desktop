/** Match the native completion target without crossing account boundaries. */
export function outboxItemId(accountId: string, attemptId: string) {
  return `outbox-${accountId}-${attemptId}`;
}
