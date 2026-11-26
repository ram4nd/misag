import type { Step } from "./flowEditor";

/** What a prompt step has the run do: send its prompt, or only wait for you. */
export type StepMode = "send" | "wait";

/**
 * What prompt step `s` has the run do; null for another kind of step. Mirrors
 * `flows::Node::mode` in src-tauri/src/flows.rs, so the editor draws a step as
 * the run will take it: a pause whose prompt is cleared only waits.
 */
export function stepMode(s: Step): StepMode | null {
  if (s.kind !== "prompt") return null;
  if (s.pause === true && s.text.trim() === "") return "wait";
  return "send";
}
