import { describe, expect, test } from "vitest";
import { shellQuote, untypeable } from "./paths";

describe("shellQuote", () => {
  test("leaves plain paths alone and single-quotes the rest", () => {
    expect(shellQuote("/p/src/main.ts")).toBe("/p/src/main.ts");
    expect(shellQuote("/p/my notes.txt")).toBe("'/p/my notes.txt'");
    expect(shellQuote("/p/it's")).toBe("'/p/it'\\''s'");
    for (const risky of ["$HOME", "`id`", "a;b", "*", "a|b", "~x", "-rf"]) {
      expect(shellQuote(risky).startsWith("'"), risky).toBe(risky !== "-rf");
    }
  });
});

describe("untypeable", () => {
  test("refuses control characters and backslashes", () => {
    expect(untypeable("/p/ok name.txt")).toBeNull();
    for (const bad of ["a\nb", "a\rb", "a\x03b", "a\x15b", "a\x1b[Ab", "a\x7fb", "a\u009bb"]) {
      expect(untypeable(bad), JSON.stringify(bad)).toContain("control characters");
    }
    expect(untypeable("a\\b")).toContain("backslash");
  });
});
