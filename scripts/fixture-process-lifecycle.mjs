function hasExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

function waitForExit(child, timeoutMs) {
  if (hasExited(child)) return Promise.resolve(true);
  return new Promise((resolve) => {
    const onExit = () => {
      clearTimeout(timer);
      resolve(true);
    };
    const timer = setTimeout(() => {
      child.off("exit", onExit);
      resolve(false);
    }, timeoutMs);
    child.once("exit", onExit);
  });
}

export async function stopFixture(
  child,
  { termTimeoutMs = 2_000, killTimeoutMs = 2_000 } = {},
) {
  if (!child || hasExited(child)) return;
  child.kill("SIGTERM");
  if (await waitForExit(child, termTimeoutMs)) return;
  child.kill("SIGKILL");
  if (await waitForExit(child, killTimeoutMs)) return;
  child.unref?.();
  throw new Error("fictional fixture did not exit after SIGKILL");
}
