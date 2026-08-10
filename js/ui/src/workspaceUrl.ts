const DEBUG_PART = "debug";

function hashBody(hash: string): string {
  return hash.startsWith("#") ? hash.slice(1) : hash;
}

function partKey(part: string): string {
  const equals = part.indexOf("=");
  const raw = equals < 0 ? part : part.slice(0, equals);
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/** Whether a workspace URL requests that the debug pane start open. */
export function debugPanelOpenFromHash(hash: string): boolean {
  return hashBody(hash)
    .split("&")
    .some((part) => partKey(part) === DEBUG_PART);
}

/**
 * Store the debug pane state in an existing raw URL fragment while leaving
 * layout, focus, share secrets, and unknown fragment parts byte-for-byte
 * unchanged. The enabled form is the established bare `debug` flag.
 */
export function withDebugPanelState(hash: string, open: boolean): string {
  const parts = hashBody(hash)
    .split("&")
    .filter((part) => part && partKey(part) !== DEBUG_PART);
  if (open) parts.push(DEBUG_PART);
  return parts.join("&");
}
