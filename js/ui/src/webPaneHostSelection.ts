/** Prefer the focused pane, then any interactive foreground pane, then dock. */
export function selectWebPaneHost<
  T extends { focused: boolean; interactive: boolean },
>(hosts: readonly T[]): T | null {
  return (
    hosts.find((host) => host.focused) ??
    hosts.find((host) => host.interactive) ??
    hosts[0] ??
    null
  );
}
