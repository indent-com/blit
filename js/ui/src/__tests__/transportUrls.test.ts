import { describe, expect, it } from "vitest";
import { muxWtUrl } from "../transportUrls";

describe("muxWtUrl", () => {
  it("resolves a port-only advertisement against the page hostname", () => {
    expect(muxWtUrl("https://blitdev.pcarrier.com/", ":10001")).toBe(
      "https://blitdev.pcarrier.com:10001/mux",
    );
  });

  it("accepts a complete advertised authority", () => {
    expect(
      muxWtUrl("https://blitdev.pcarrier.com/", "gateway.example:4443"),
    ).toBe("https://gateway.example:4443/mux");
  });

  it("uses the page authority when the gateway does not advertise a URL", () => {
    expect(muxWtUrl("https://blitdev.pcarrier.com/")).toBe(
      "https://blitdev.pcarrier.com/mux",
    );
  });

  it("preserves a nested base path and strips page-only URL state", () => {
    expect(muxWtUrl("https://example.test:7443/base?dark#terminal")).toBe(
      "https://example.test:7443/base/mux",
    );
  });
});
