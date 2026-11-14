import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

const backend = vi.hoisted(() => ({
  /** What flow_start returns. */
  started: { runId: 7, clear: false, prompt: "Review the diff" as string | null },
  calls: [] as string[],
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string) => {
    backend.calls.push(cmd);
    if (cmd === "flow_start") return backend.started;
    if (cmd === "flow_runs") return [];
    if (cmd === "flow_continue") return "Fix what the review found";
    return null;
  }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ ask: vi.fn(async () => true) }));

import { createRunController, type RunController } from "./runController";
import type { Flow, Run } from "./flows";

interface Tab {
  tabId: string;
}

const decoder = new TextDecoder();
let typed: string[];
let locks: boolean[];
let runs: RunController<Tab>;
const tab: Tab = { tabId: "t1" };

beforeEach(() => {
  vi.useFakeTimers();
  typed = [];
  locks = [];
  backend.calls = [];
  backend.started = { runId: 7, clear: false, prompt: "Review the diff" };
  runs = createRunController<Tab>({
    flows: { showRuns: () => {}, showBoxError: () => {}, reload: async () => {} },
    errorText: String,
    tabs: () => [tab],
    active: () => tab,
    label: () => "tab 1",
    busy: () => false,
    live: async () => true,
    write: (_t, data) => typed.push(decoder.decode(data)),
    editors: () => [],
    markWaiting: () => {},
    lockChanged: (_t, locked) => locks.push(locked),
  });
});

afterEach(() => {
  vi.useRealTimers();
});

const flow = { id: 1, name: "Review", unconfirmed: false } as Flow;

describe("a flow sending a prompt", () => {
  test("clears what the person typed first", async () => {
    runs.typed(tab, "half a thought");
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(typed).toEqual(["\x03", "\x1b[200~Review the diff\x1b[201~", "\r"]);
  });

  test("leaves an input the person did not type in alone", async () => {
    runs.typed(tab, "sent\r");
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(typed).toEqual(["\x1b[200~Review the diff\x1b[201~", "\r"]);
  });

  test("locks the tab's keyboard until the prompt is sent", async () => {
    const done = runs.start(flow);
    await vi.advanceTimersByTimeAsync(0);
    expect(runs.locked(tab)).toBe(true);
    await vi.runAllTimersAsync();
    await done;
    expect(runs.locked(tab)).toBe(false);
    expect(locks).toEqual([true, false]);
  });

  test("clears and locks when going on from a step as well", async () => {
    runs.typed(tab, "x");
    const done = runs.proceed({ id: 7, tabId: "t1", stepId: "s", flowId: 1, status: "waiting" } as unknown as Run, "ok");
    await vi.advanceTimersByTimeAsync(0);
    expect(runs.locked(tab)).toBe(true);
    await vi.runAllTimersAsync();
    expect(await done).toBeNull();
    expect(typed).toEqual(["\x03", "\x1b[200~Fix what the review found\x1b[201~", "\r"]);
    expect(runs.locked(tab)).toBe(false);
  });
});
