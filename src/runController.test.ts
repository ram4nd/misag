import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

const backend = vi.hoisted(() => ({
  /** What flow_start returns. */
  started: { runId: 7, clear: false, prompt: "Review the diff" as string | null },
  calls: [] as string[],
  /** The arguments of each flow_unsent call. */
  unsent: [] as unknown[],
  /** What flow_unsent returns, or the error it fails with. */
  unsentReply: true as boolean | Error,
  /** What the next flow_runs calls answer, one each, in order; then `runs`. */
  runsReplies: [] as Promise<unknown[]>[],
  /** What flow_runs returns once `runsReplies` is used up. */
  runs: [] as unknown[],
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args?: unknown) => {
    backend.calls.push(cmd);
    if (cmd === "flow_unsent") {
      backend.unsent.push(args);
      if (backend.unsentReply instanceof Error) throw backend.unsentReply;
      return backend.unsentReply;
    }
    if (cmd === "flow_start") return backend.started;
    if (cmd === "flow_runs") return backend.runsReplies.shift() ?? backend.runs;
    if (cmd === "flow_prefill_text") return { text: "Fix the issues", token: 1 };
    if (cmd === "flow_continue") return { stepId: "fix", prompt: "Fix what the review found", briefs: true };
    if (cmd === "flow_resume") return { stepId: "s", prompt: "Go on with the step", briefs: false };
    if (cmd === "flow_cleared") return { state: "gone" };
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
let busy: boolean;
let background: boolean;
let live: () => Promise<boolean>;
let write: (text: string) => void | Promise<void>;
/** Called as a flow takes or releases the tab's lock. */
let onLock: () => void;
const tab: Tab = { tabId: "t1" };
/** The ids of the runs shown each time the list is drawn. */
let shownRuns: number[][];

beforeEach(() => {
  vi.useFakeTimers();
  typed = [];
  locks = [];
  busy = false;
  background = false;
  live = async () => true;
  write = (text) => void typed.push(text);
  onLock = () => {};
  backend.calls = [];
  backend.unsent = [];
  backend.runs = [];
  backend.unsentReply = true;
  backend.runsReplies = [];
  shownRuns = [];
  backend.started = { runId: 7, clear: false, prompt: "Review the diff" };
  runs = createRunController<Tab>({
    flows: { showRuns: (here) => shownRuns.push(here.map((r) => r.id)), showBoxError: () => {}, reload: async () => {} },
    errorText: String,
    tabs: () => [tab],
    active: () => tab,
    label: () => "tab 1",
    busy: () => busy,
    waitsInBackground: () => background,
    live: (_t) => live(),
    write: (_t, data) => write(decoder.decode(data)),
    editors: () => [],
    markWaiting: () => {},
    lockChanged: (_t, locked) => {
      locks.push(locked);
      onLock();
    },
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
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

  test("a focus report from the terminal does not count as typing", async () => {
    runs.typed(tab, "\x1b[O");
    runs.typed(tab, "\x1b[I");
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(typed).toEqual(["\x1b[200~Review the diff\x1b[201~", "\r"]);
  });

  test("a focus report leaves what the person typed counted", async () => {
    runs.typed(tab, "a");
    runs.typed(tab, new TextEncoder().encode("\x1b[O"));
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(typed[0]).toBe("\x03");
  });

  test("an Enter followed by a focus report still sends the input", async () => {
    runs.typed(tab, "half a thought");
    runs.typed(tab, "\r\x1b[O");
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

describe("going on from a step", () => {
  const run = { id: 7, tabId: "t1", stepId: "s", flowId: 1, status: "running" } as unknown as Run;
  const working = "Claude is working in tab 1; interrupt it (Esc) or wait for its reply, then go on, or stop the run with ■.";
  const waiting =
    "Claude in tab 1 is waiting on background tasks, which wake it for another turn; go on once they finish, or stop the run with ■.";

  test("going on while Claude is busy types nothing and says why", async () => {
    busy = true;
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(working);
    const resumed = runs.resume(run);
    await vi.runAllTimersAsync();
    expect(await resumed).toBe(working);
    expect(backend.calls).not.toContain("flow_continue");
    expect(backend.calls).not.toContain("flow_resume");
    expect(backend.calls).toContain("flow_runs");
    expect(typed).toEqual([]);
    expect(locks).toEqual([]);
  });

  test("a waiting run is refused too while Claude is busy", async () => {
    busy = true;
    const picked = runs.proceed({ ...run, status: "manual" }, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(working);
    expect(backend.calls).not.toContain("flow_continue");
    expect(typed).toEqual([]);
  });

  test("going on while Claude waits on background tasks types nothing and says why", async () => {
    background = true;
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(waiting);
    expect(backend.calls).not.toContain("flow_continue");
    expect(backend.calls).toContain("flow_runs");
    expect(typed).toEqual([]);
  });

  test("Claude not running wins over Claude busy", async () => {
    busy = true;
    live = async () => false;
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe("Claude is not running in tab 1; start it there first, or stop the run.");
  });

  test("Claude starting to work while the pick waits for its check is refused", async () => {
    live = async () => {
      // Claude's title updates while the check is under way.
      busy = true;
      return true;
    };
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(working);
    expect(backend.calls).not.toContain("flow_continue");
    expect(typed).toEqual([]);
  });

  test("Claude starting to work as the pick takes the tab's lock is refused", async () => {
    onLock = () => {
      busy = true;
    };
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(working);
    expect(backend.calls).not.toContain("flow_continue");
    expect(typed).toEqual([]);
    expect(locks).toEqual([true, false]);
  });

  test("goes on once Claude is idle again", async () => {
    busy = true;
    const refused = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    await refused;
    busy = false;
    const done = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await done).toBeNull();
    expect(backend.calls).toContain("flow_continue");
    expect(typed).toEqual(["\x1b[200~Fix what the review found\x1b[201~", "\r"]);
  });

  const left = "Claude left the run's tab before this step's prompt was sent; start Claude there and press Continue to send it again.";

  /** Claude runs for the first `n` checks, then has left the tab. */
  function leavesAfter(n: number) {
    let checks = 0;
    live = async () => ++checks <= n;
  }

  test("a step prompt Claude left the tab before makes the run wait at the new step for Continue", async () => {
    // Claude runs when the pick is checked, and has left by the time the prompt is typed.
    leavesAfter(1);
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    // The run's note says so, in the run bar and the Flows box.
    expect(await picked).toBeNull();
    expect(backend.unsent).toEqual([{ runId: 7, stepId: "fix", briefs: true, why: left }]);
    expect(typed).toEqual([]);
  });

  test("Continue's prompt Claude left the tab before names the step flow_resume gave", async () => {
    leavesAfter(1);
    const resumed = runs.resume({ ...run, status: "attention" } as Run);
    await vi.runAllTimersAsync();
    expect(await resumed).toBeNull();
    expect(backend.calls).toContain("flow_resume");
    expect(backend.unsent).toEqual([{ runId: 7, stepId: "s", briefs: false, why: left }]);
  });

  test("a prompt that could not be typed at all makes the run wait as well", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    write = (text) => {
      if (text.startsWith("\x1b[200~")) throw new Error("the pty is gone");
      typed.push(text);
    };
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBeNull();
    expect(backend.unsent).toEqual([
      {
        runId: 7,
        stepId: "fix",
        briefs: true,
        why: "This step's prompt could not be typed into the run's tab (Error: the pty is gone); press Continue to send it again.",
      },
    ]);
    expect(runs.locked(tab)).toBe(false);
  });

  test("a run that could not be made to wait says so", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    leavesAfter(1);
    backend.unsentReply = new Error("database is locked");
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe(
      "The step's prompt was not sent, and the run could not be made to wait (Error: database is locked): pick an output to go on, or stop it with ■.",
    );
  });

  test("a run that moved on meanwhile is not said to wait", async () => {
    leavesAfter(1);
    backend.unsentReply = false;
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBe("The step's prompt was not sent, and the run had already moved on.");
  });

  test("a paste Claude left before its Enter is cleared before the next prompt", async () => {
    leavesAfter(2);
    const picked = runs.proceed(run, "ok");
    await vi.runAllTimersAsync();
    expect(await picked).toBeNull();
    expect(typed).toEqual(["\x1b[200~Fix what the review found\x1b[201~"]);
    typed = [];
    live = async () => true;
    const resumed = runs.resume({ ...run, status: "attention" } as Run);
    await vi.runAllTimersAsync();
    expect(await resumed).toBeNull();
    expect(typed).toEqual(["\x03", "\x1b[200~Go on with the step\x1b[201~", "\r"]);
  });
});

describe("starting a flow", () => {
  test("is refused while Claude waits on background tasks, as their turn would be taken for the briefing's reply", async () => {
    background = true;
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(backend.calls).not.toContain("flow_start");
    expect(typed).toEqual([]);
  });

  test("one that clears still starts while Claude waits on background tasks", async () => {
    background = true;
    backend.started = { runId: 7, clear: true, prompt: null };
    const done = runs.start({ ...flow, clear: true });
    await vi.advanceTimersByTimeAsync(1000);
    expect(backend.calls).toContain("flow_start");
    expect(typed).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
    await vi.runAllTimersAsync();
    await done;
  });
});

describe("a manual prompt's draft", () => {
  const run = { id: 8, tabId: "t1", stepId: "m", step: "Manual prompt", flowId: 1, status: "manual", prefill: true } as unknown as Run;

  test("goes in after a focus report from clicking the run bar", async () => {
    runs.typed(tab, "\x1b[O");
    backend.runs = [run];
    await runs.poll();
    await vi.runAllTimersAsync();
    expect(typed).toEqual(["\x1b[200~Fix the issues\x1b[201~"]);
  });

  test("is still taken back out on stop after a focus report", async () => {
    backend.runs = [run];
    await runs.poll();
    await vi.runAllTimersAsync();
    backend.runs = [];
    runs.typed(tab, "\x1b[O");
    typed = [];
    const stopped = runs.stop(run);
    await vi.runAllTimersAsync();
    await stopped;
    expect(typed).toEqual(["\x03"]);
  });
});

describe("reading the runs", () => {
  test("a reply that comes in after a newer one is dropped", async () => {
    let answerOld!: (runs: unknown[]) => void;
    backend.runsReplies = [new Promise((resolve) => (answerOld = resolve)), Promise.resolve([])];
    const old = runs.poll();
    await runs.poll();
    expect(shownRuns).toEqual([[]]);
    answerOld([{ id: 7, tabId: "t1", flowId: 1, stepId: "s", status: "manual", outputs: ["ok"] }]);
    await old;
    // The newer reply no longer has the run (it moved on): the older reply must not bring it back.
    expect(shownRuns).toEqual([[]]);
    expect(runs.waiting(tab)).toBe(false);
  });
});

describe("a tab that does not take what is typed", () => {
  const refuse = () => {
    // As the tab does: the write is sent, and fails later.
    write = () => Promise.reject(new Error("its program is not reading input"));
  };

  test("a step's prompt it refuses is not submitted, and the run waits for the person", async () => {
    refuse();
    const done = runs.proceed({ id: 7, tabId: "t1", stepId: "s", flowId: 1, status: "waiting" } as unknown as Run, "ok");
    await vi.runAllTimersAsync();
    expect(await done).toBeNull();
    expect(backend.calls).toContain("flow_unsent");
    expect(JSON.stringify(backend.unsent)).toContain("could not be typed");
  });

  test("a briefing it refuses stops the run, saying why", async () => {
    refuse();
    const done = runs.start(flow);
    await vi.runAllTimersAsync();
    await done;
    expect(backend.calls).toContain("flow_stop");
  });

  test("a manual prompt's draft it refuses is not recorded as pasted", async () => {
    refuse();
    backend.runs = [{ id: 8, tabId: "t1", stepId: "m", step: "Manual prompt", flowId: 1, status: "manual", prefill: true }];
    await runs.poll();
    await vi.runAllTimersAsync();
    expect(backend.calls).toContain("flow_prefill_text");
    expect(backend.calls).not.toContain("flow_prefilled");
  });
});

