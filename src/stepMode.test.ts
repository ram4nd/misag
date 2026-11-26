import { describe, expect, test } from "vitest";
import { stepMode } from "./stepMode";
import type { Step } from "./flowEditor";

const step = (over: Partial<Step>): Step => ({ id: "s", kind: "prompt", title: "", text: "", outputs: ["done"], x: 0, y: 0, ...over });

describe("stepMode mirrors flows::Node::mode", () => {
  test("a prompt sends, writes your draft, or only waits", () => {
    expect(stepMode(step({ text: "Fix it" }))).toBe("send");
    expect(stepMode(step({ text: "Fix it", pause: true }))).toBe("send");
    expect(stepMode(step({ pause: true, text: "  " }))).toBe("wait");
    expect(stepMode(step({ draft: true }))).toBe("draft");
    expect(stepMode(step({ draft: true, pause: true }))).toBe("draft");
    expect(stepMode(step({ text: "" }))).toBe("send");
  });

  test("other steps have no mode", () => {
    expect(stepMode(step({ kind: "title" }))).toBeNull();
    expect(stepMode(step({ kind: "start", pause: true }))).toBeNull();
  });
});
