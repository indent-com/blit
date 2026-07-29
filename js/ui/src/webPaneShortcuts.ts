/** Relay workspace pane-removal chords from a same-origin web-pane iframe. */
export function forwardWebPaneCloseShortcut(
  event: KeyboardEvent,
  claimFocus: () => void,
  target: EventTarget = window,
): boolean {
  if (
    !event.ctrlKey ||
    !event.shiftKey ||
    event.metaKey ||
    (event.key !== "Q" && event.key !== "q" && event.code !== "KeyQ")
  ) {
    return false;
  }

  claimFocus();
  const forwarded = new KeyboardEvent("keydown", {
    key: event.key,
    code: event.code,
    ctrlKey: true,
    altKey: event.altKey,
    shiftKey: true,
    bubbles: true,
    cancelable: true,
  });
  target.dispatchEvent(forwarded);
  if (forwarded.defaultPrevented) {
    event.preventDefault();
    event.stopPropagation();
  }
  return true;
}
