import { describe, expect, it } from "vitest";
import { CookieJar } from "../cookies";

describe("CookieJar HttpOnly", () => {
  // The preview must not give an app a *weaker* cookie contract than its
  // real origin does. The jar used to ignore HttpOnly entirely while the
  // injected document.cookie shim returned everything in it, so a dev
  // server's HttpOnly session cookie became readable by any script on the
  // page — the whole property the attribute exists to provide.
  it("withholds HttpOnly cookies from script but sends them upstream", () => {
    const jar = new CookieJar();
    jar.set("sid=secret; Path=/; HttpOnly", "/");
    jar.set("theme=dark; Path=/", "/");

    const upstream = jar.header("/app");
    expect(upstream).toContain("sid=secret");
    expect(upstream).toContain("theme=dark");

    const forScript = jar.header("/app", true);
    expect(forScript).toContain("theme=dark");
    expect(forScript).not.toContain("sid");
    expect(forScript).not.toContain("secret");
  });

  it("recognises HttpOnly however it is spelled or ordered", () => {
    for (const header of [
      "a=1; httponly",
      "a=1; HTTPONLY",
      "a=1; HttpOnly; Path=/",
      "a=1; Path=/; HttpOnly",
    ]) {
      const jar = new CookieJar();
      jar.set(header, "/");
      expect(jar.header("/", true), header).toBeUndefined();
      expect(jar.header("/"), header).toBe("a=1");
    }
  });

  it("leaves an all-HttpOnly jar with nothing to say to script", () => {
    const jar = new CookieJar();
    jar.set("only=1; HttpOnly", "/");
    // undefined, not an empty string: the shim distinguishes "no cookies"
    // from a header it should send.
    expect(jar.header("/", true)).toBeUndefined();
  });

  it("does not treat an ordinary cookie as HttpOnly", () => {
    const jar = new CookieJar();
    // A value that merely mentions the word must not trip the parser.
    jar.set("note=httponly; Path=/", "/");
    expect(jar.header("/", true)).toBe("note=httponly");
  });

  it("keeps path scoping and expiry independent of the split", () => {
    const jar = new CookieJar();
    jar.set("deep=1; Path=/admin; HttpOnly", "/");
    jar.set("wide=2; Path=/", "/");
    expect(jar.header("/", true)).toBe("wide=2");
    expect(jar.header("/admin")).toContain("deep=1");
    expect(jar.header("/admin", true)).toBe("wide=2");

    jar.set("gone=3; Path=/; Max-Age=0", "/");
    expect(jar.header("/")).not.toContain("gone");
  });
});
