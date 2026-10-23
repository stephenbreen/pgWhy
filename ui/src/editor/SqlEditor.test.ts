import { describe, expect, it } from "vitest";
import { byteToCharOffset, charToByteOffset } from "./SqlEditor";

describe("offset conversion", () => {
  it("maps UTF-8 byte offsets to UTF-16 offsets across multibyte text", () => {
    const text = "select 'é漢😀' frm t";
    const charIdx = text.indexOf("frm");
    const byteIdx = new TextEncoder().encode(text.slice(0, charIdx)).length;
    expect(byteIdx).toBeGreaterThan(charIdx);
    expect(byteToCharOffset(text, byteIdx)).toBe(charIdx);
    expect(charToByteOffset(text, charIdx)).toBe(byteIdx);
  });

  it("clamps past the end", () => {
    expect(byteToCharOffset("abc", 99)).toBe(3);
  });
});
