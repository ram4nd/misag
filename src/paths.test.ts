import { describe, expect, test } from "vitest";
import { folderLabel, notLocal, shellQuote, untypeable } from "./paths";

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

describe("notLocal", () => {
  test("refuses links, remote files, relative paths and names read lossily", () => {
    expect(notLocal("/p/ok name.txt")).toBeNull();
    for (const remote of ["https://example.com/x", "sftp://host/x", "trash:///x", "host/x"]) {
      expect(notLocal(remote), remote).toContain("not a local file");
    }
    expect(notLocal("/p/\ufffd.txt")).toContain("could not be read");
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

describe("folderLabel", () => {
  test("is the last folder of the path, ~ for home and / for the root", () => {
    expect(folderLabel("/home/me/src/misag", "/home/me")).toBe("misag");
    expect(folderLabel("/home/me/src/misag/", "/home/me")).toBe("misag");
    expect(folderLabel("/home/me", "/home/me")).toBe("~");
    expect(folderLabel("/home/me/", "/home/me")).toBe("~");
    expect(folderLabel("/home/me", null)).toBe("me");
    expect(folderLabel("/", "/home/me")).toBe("/");
  });
});

describe("on Windows", () => {
  test("quotes for PowerShell", () => {
    expect(shellQuote("C:\\p\\src\\main.ts", true)).toBe("C:\\p\\src\\main.ts");
    expect(shellQuote("C:\\p\\my notes.txt", true)).toBe("'C:\\p\\my notes.txt'");
    expect(shellQuote("C:\\p\\it's", true)).toBe("'C:\\p\\it''s'");
    for (const risky of ["$HOME", "`id`", "a;b", "@x", "a,b", "(x)", "{x}"]) {
      expect(shellQuote(risky, true), risky).toBe(`'${risky}'`);
    }
  });

  test("types backslashes but not control characters", () => {
    expect(untypeable("C:\\p\\x.txt", true)).toBeNull();
    expect(untypeable("C:\\p\\a\nb", true)).toContain("control characters");
  });

  test("takes drive paths as local and network paths and links as not", () => {
    expect(notLocal("C:\\p\\x.txt", true)).toBeNull();
    expect(notLocal("d:/p/x.txt", true)).toBeNull();
    for (const remote of ["\\\\server\\share\\x", "https://example.com/x", "/p/x", "p\\x"]) {
      expect(notLocal(remote, true), remote).toContain("not a local file");
    }
  });

  test("labels folders split by either slash", () => {
    expect(folderLabel("C:\\Users\\ra\\src\\misag", "C:\\Users\\ra", true)).toBe("misag");
    expect(folderLabel("C:\\Users\\ra\\", "C:\\Users\\ra", true)).toBe("~");
    expect(folderLabel("C:\\", null, true)).toBe("C:\\");
    expect(folderLabel("C:/Users/ra/x", null, true)).toBe("x");
  });
});
