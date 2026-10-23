import {
  onMailArrived,
  onMailChanged,
  onMailHydrated,
  onMailSyncState,
} from "./nativeWindows";
import type { MailArrival, MailHydrated } from "./types";
import { subscribeNativeGroup } from "./nativeSubscriptions";

type Unlisten = () => void;

export type RealtimeEventHandlers = {
  arrived: (arrival: MailArrival) => void;
  changed: (accountId: string) => void;
  hydrated: (hydrated: MailHydrated) => void;
  syncState: () => void;
};

/**
 * Subscribes the main frontend to every event published by the native
 * realtime watcher. Keeping this wiring together makes the native event
 * contract testable without mounting the whole mailbox application.
 */
export async function subscribeRealtimeEvents(
  handlers: RealtimeEventHandlers,
  signal?: AbortSignal,
): Promise<Unlisten> {
  return subscribeNativeGroup(
    [
      onMailArrived(handlers.arrived),
      onMailHydrated(handlers.hydrated),
      onMailChanged(handlers.changed),
      onMailSyncState(handlers.syncState),
    ],
    signal,
  );
}
