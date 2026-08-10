/** Build the WebTransport URL for the multiplexed endpoint.
 *
 * WebTransport always uses HTTPS/QUIC. A `:port` advertisement keeps the
 * browser page's hostname; `hostname:port` replaces the whole authority.
 */
export function muxWtUrl(
  href: string = location.href,
  advertisedAddr?: string,
): string {
  const url = new URL(href);
  url.protocol = "https:";
  if (advertisedAddr?.startsWith(":")) {
    url.port = advertisedAddr.slice(1);
  } else if (advertisedAddr) {
    url.host = advertisedAddr;
  }
  url.pathname = url.pathname.endsWith("/")
    ? url.pathname + "mux"
    : url.pathname + "/mux";
  url.search = "";
  url.hash = "";
  return url.href;
}
