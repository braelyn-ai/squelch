// Pure-logic tests for the 2FA code extractor. Run with: `bun test`.
// No DOM needed — extractCode is pure string work.

import { expect, test, describe } from "bun:test";
import { extractCode, isCodeKind } from "./authCode";

describe("extractCode", () => {
  test("pulls a code sitting next to a code word", () => {
    expect(extractCode("Your verification code is 483920. It expires soon.")).toBe(
      "483920",
    );
  });

  test("prefers the code-word-adjacent run over an unrelated number", () => {
    const body =
      "Order #100482 shipped. Your login code is 55231 — do not share it.";
    expect(extractCode(body)).toBe("55231");
  });

  test("handles a space/hyphen split code", () => {
    expect(extractCode("Your one-time passcode: 123 456")).toBe("123456");
    expect(extractCode("PIN: 12-34-56")).toBe("123456");
  });

  test("falls back to the longest standalone run when no code word", () => {
    expect(extractCode("Reference 4821 / confirmation 9930012")).toBe("9930012");
  });

  test("returns null when nothing plausible exists", () => {
    expect(extractCode("Hi Sam, thanks for signing up! Cheers.")).toBeNull();
    expect(extractCode("")).toBeNull();
    expect(extractCode(null)).toBeNull();
  });

  test("does not grab a long id / year fragment as a 4-8 run inside words", () => {
    // The 11-digit id is >8 so it's rejected; the code word wins.
    expect(
      extractCode("Session id ab12345678901. Your code 90210 is valid."),
    ).toBe("90210");
  });
});

describe("isCodeKind", () => {
  test("code kinds pop the modal", () => {
    expect(isCodeKind("otp")).toBe(true);
    expect(isCodeKind("login_code")).toBe(true);
    expect(isCodeKind("verification")).toBe(true);
  });
  test("non-code kinds get the ring only", () => {
    expect(isCodeKind("password_reset")).toBe(false);
    expect(isCodeKind("login_alert")).toBe(false);
    expect(isCodeKind("magic_link")).toBe(false);
    expect(isCodeKind(null)).toBe(false);
    expect(isCodeKind(undefined)).toBe(false);
  });
});
