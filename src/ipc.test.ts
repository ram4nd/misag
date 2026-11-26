import { describe, expect, test } from "vitest";
import fixtures from "./ipc.fixtures.json";
import type { Flow, Run } from "./flows";
import type { Page, Past } from "./history";
import type { Cleared, Prefill, Started, TabTitle } from "./runController";

/**
 * Each mirror's fields, listed so the compiler checks the list is complete and has
 * no extra ones. The fixtures are the values the backend sends, written by a Rust
 * test (`the_page_s_mirrors_match_the_values_sent` in src-tauri/src/lib.rs).
 */
const RUN: Record<keyof Run, true> = {
  id: true, flowId: true, flowName: true, tabId: true, stepId: true, step: true,
  status: true, note: true, outputs: true, steps: true, done: true, total: true, resumable: true, prefill: true, stepKind: true, briefing: true,
};
const PREFILL: Record<keyof Prefill, true> = { text: true, token: true };
const TAB_TITLE: Record<keyof TabTitle, true> = { tabId: true, title: true };
const STARTED: Record<keyof Started, true> = { runId: true, clear: true, prompt: true };
const FLOW: Record<keyof Flow, true> = { id: true, name: true, archived: true, clear: true, unconfirmed: true };
const PAGE: Record<keyof Page, true> = { sessions: true, more: true, unreadable: true };
const PAST: Record<keyof Past, true> = { id: true, title: true, cwd: true, modified: true, pinned: true };

const fields = (o: object) => Object.keys(o).sort();

describe("the page's types mirror the values the backend sends", () => {
  test.each([
    ["Run", fixtures.run, RUN],
    ["Started", fixtures.started, STARTED],
    ["Flow", fixtures.flow, FLOW],
    ["Prefill", fixtures.prefill, PREFILL],
    ["TabTitle", fixtures.tabTitle, TAB_TITLE],
    ["Page", fixtures.page, PAGE],
    ["Past", fixtures.page.sessions[0], PAST],
  ])("%s", (_, value, mirror) => {
    expect(fields(value)).toEqual(fields(mirror));
  });

  test("Cleared", () => {
    // Assignable only if every state the backend sends is one the page knows.
    const states: Cleared[] = fixtures.cleared.map((c) => c as Cleared);
    expect(states.map((c) => c.state)).toEqual(["pending", "ready", "gone"]);
    expect(fields(fixtures.cleared[1])).toEqual(["prompt", "state"]);
  });
});
