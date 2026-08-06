/**
 * Whether a chord pressed inside a web pane belongs to the workspace rather
 * than to the page. Deliberately short: the two things a pane must never be
 * able to swallow are the chords that remove it and the chords that move focus
 * off it — without those, a page that takes the keyboard is a pane you cannot
 * leave.
 */
function isWorkspaceChord(event: KeyboardEvent): boolean {
  if (event.metaKey) return false;
  // Ctrl+Shift+Q (park) and Ctrl+Alt+Shift+Q (close). Accept `code` too:
  // Alt rewrites the key value on a Mac.
  if (
    event.ctrlKey &&
    event.shiftKey &&
    (event.key === "Q" || event.key === "q" || event.code === "KeyQ")
  ) {
    return true;
  }
  // Alt+Shift+[ / ] — prev/next window. Same `code` reasoning: Alt turns
  // [ and ] into " and ' on a Mac.
  return (
    event.altKey &&
    event.shiftKey &&
    !event.ctrlKey &&
    (event.code === "BracketLeft" || event.code === "BracketRight")
  );
}

/** Relay workspace chords from a same-origin web-pane iframe. */
export function forwardWebPaneWorkspaceShortcut(
  event: KeyboardEvent,
  claimFocus: () => void,
  target: EventTarget = window,
): boolean {
  if (!isWorkspaceChord(event)) return false;

  claimFocus();
  const forwarded = new KeyboardEvent("keydown", {
    key: event.key,
    code: event.code,
    ctrlKey: event.ctrlKey,
    altKey: event.altKey,
    shiftKey: event.shiftKey,
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
