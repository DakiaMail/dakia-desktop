export type Unlisten = () => void;

/**
 * Registers a related set of native listeners as one lifecycle boundary.
 * The first registration failure rejects promptly, cleans listeners that have
 * already succeeded, and makes any later successful registration self-clean.
 */
export function subscribeNativeGroup(
  registrations: Array<Promise<Unlisten>>,
  signal?: AbortSignal,
): Promise<Unlisten> {
  return new Promise((resolve, reject) => {
    const listeners: Unlisten[] = [];
    let remaining = registrations.length;
    let closed = false;

    const cleanupOne = (unlisten: Unlisten) => {
      try {
        unlisten();
      } catch {
        // Listener teardown must not strand sibling native registrations.
      }
    };
    const cleanup = () => {
      for (const unlisten of listeners.splice(0)) {
        cleanupOne(unlisten);
      }
    };
    const finish = () => signal?.removeEventListener("abort", cancel);
    const cancel = () => {
      if (closed) return;
      closed = true;
      cleanup();
      finish();
      resolve(() => undefined);
    };

    if (!remaining) {
      finish();
      resolve(() => undefined);
      return;
    }

    for (const registration of registrations) {
      void registration.then(
        (unlisten) => {
          if (closed) {
            cleanupOne(unlisten);
            return;
          }
          listeners.push(unlisten);
          remaining -= 1;
          if (remaining === 0) {
            resolve(cancel);
          }
        },
        (error) => {
          if (closed) return;
          closed = true;
          cleanup();
          finish();
          reject(error);
        },
      );
    }

    if (signal?.aborted) cancel();
    else signal?.addEventListener("abort", cancel, { once: true });
  });
}
