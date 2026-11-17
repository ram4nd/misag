import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import type { WTerm } from "@wterm/dom";
import { createTabPoller, type TabPoller } from "./tabPoller";
import type { Session } from "./tabState";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const ipc = vi.mocked(invoke);

/** How long a `claude` must run without reporting before it is taken to bypass the wrapper; mirrors UNTRACKED_GRACE_MS in tabPoller.ts. */
const UNTRACKED_GRACE_MS = 10_000;

/** A running terminal tab with PTY `id`. */
function session(id: number, over: Partial<Session> = {}): Session {
  return {
    tabId: `t${id}`, run: { kind: "terminal", command: null, claudeSession: null },
    id, starts: 1, name: `shell ${id}`, renamed: false, busy: false, background: null, turnStarted: 0,
    shownBusy: false, finishSettle: null, finishAfter: null, term: {} as WTerm, el: document.createElement("div"),
    exited: false, closing: false, skipsPermissions: false, inClaude: false, claudeOpen: null, resumingUntil: 0,
    ...over,
  };
}

let list: Session[];
let lifetime: AbortController;

function stubOptions(signal: AbortSignal) {
  return {
    signal,
    notices: { set: vi.fn() },
    tabs: () => list,
    updateTab: vi.fn(),
    showHistory: vi.fn(),
    scheduleSave: vi.fn(),
    playFinish: vi.fn(),
  };
}
let opts: ReturnType<typeof stubOptions>;
let poller: TabPoller;

/** Answers each command with `answers[cmd]`; one given as a promise holds the check until it settles. */
function answer(answers: Record<string, unknown>) {
  ipc.mockImplementation(async (cmd) => {
    if (!(cmd in answers)) throw new Error(`unmocked ${cmd}`);
    return answers[cmd];
  });
}

/** A promise and the function that settles it with `value`. */
function held<T>(value: T): [Promise<T>, () => void] {
  let release!: () => void;
  const promise = new Promise<T>((r) => (release = () => r(value)));
  return [promise, release];
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date", "setTimeout", "clearTimeout"] });
  ipc.mockReset();
  list = [];
  lifetime = new AbortController();
  opts = stubOptions(lifetime.signal);
  poller = createTabPoller(opts);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("followClaude", () => {
  test("a check that cannot tell keeps the session the tab has open", () => {
    const s = session(1, { claudeOpen: "a", inClaude: true });
    poller.followClaude(s, { state: "unknown" });
    poller.followClaude(s, undefined);
    expect(s.claudeOpen).toBe("a");
    expect(s.inClaude).toBe(true);
    expect(opts.updateTab).not.toHaveBeenCalled();
  });

  test("an open Claude is followed, and a closed one forgotten", () => {
    const s = session(1);
    poller.followClaude(s, { state: "open", session: "a", skipsPermissions: true });
    expect([s.claudeOpen, s.inClaude, s.skipsPermissions]).toEqual(["a", true, true]);
    expect(opts.updateTab).toHaveBeenCalledWith(s);
    poller.followClaude(s, { state: "closed" });
    expect([s.claudeOpen, s.inClaude, s.skipsPermissions]).toEqual([null, false, false]);
  });

  test("a closed shell does not forget the session while a resumed Claude may still be starting", () => {
    const s = session(1, { claudeOpen: "a", resumingUntil: Date.now() + 1_000 });
    poller.followClaude(s, { state: "closed" });
    expect(s.claudeOpen).toBe("a");
    vi.advanceTimersByTime(1_000);
    poller.followClaude(s, { state: "closed" });
    expect(s.claudeOpen).toBeNull();
  });

  test("an open Claude ends the resume wait", () => {
    const s = session(1, { claudeOpen: "a", resumingUntil: Date.now() + 1_000 });
    poller.followClaude(s, { state: "open", session: "a", skipsPermissions: false });
    expect(s.resumingUntil).toBe(0);
  });

  test("an exited tab has no Claude, whatever an earlier check says", () => {
    const s = session(1, { claudeOpen: "a", exited: true });
    poller.followClaude(s, { state: "open", session: "a", skipsPermissions: false });
    expect(s.claudeOpen).toBeNull();
  });

  test("a claude that never reports is noted only after the grace time, and a new one starts it again", () => {
    const s = session(1);
    poller.followClaude(s, { state: "untracked", pid: 5, skipsPermissions: false });
    expect(s.untracked).toEqual({ pid: 5, since: Date.now() });
    vi.advanceTimersByTime(UNTRACKED_GRACE_MS - 1);
    poller.followClaude(s, { state: "untracked", pid: 5, skipsPermissions: false });
    expect(opts.notices.set).not.toHaveBeenCalled();
    // Another claude: its own grace time.
    poller.followClaude(s, { state: "untracked", pid: 6, skipsPermissions: false });
    vi.advanceTimersByTime(UNTRACKED_GRACE_MS - 1);
    poller.followClaude(s, { state: "untracked", pid: 6, skipsPermissions: false });
    expect(opts.notices.set).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    poller.followClaude(s, { state: "untracked", pid: 6, skipsPermissions: false });
    expect(opts.notices.set).toHaveBeenCalledWith("tab:t1:untracked", expect.stringContaining("shell 1"), true, "6");
    poller.followClaude(s, { state: "closed" });
    expect(s.untracked).toBeUndefined();
  });
});

describe("poll", () => {
  test("a slow check is not started again until it returns", async () => {
    const s = session(1);
    list = [s];
    const [cwds, release] = held({ 1: "/a" });
    answer({ pty_cwds: cwds, claude_open_sessions: {}, background_tabs: {} });
    const first = poller.poll();
    await poller.poll();
    expect(ipc.mock.calls.filter(([cmd]) => cmd === "pty_cwds")).toHaveLength(1);
    release();
    await first;
    expect(s.liveCwd).toBe("/a");
    expect(opts.updateTab).toHaveBeenCalledWith(s);
    expect(opts.showHistory).toHaveBeenCalledTimes(1);
    expect(opts.scheduleSave).toHaveBeenCalledTimes(1);
    answer({ pty_cwds: { 1: "/b" }, claude_open_sessions: {}, background_tabs: {} });
    await poller.poll();
    expect(s.liveCwd).toBe("/b");
    expect(poller.checks()).toBe(2);
  });

  test("answers for a tab that restarted, exited or closed meanwhile are dropped", async () => {
    const restarted = session(1);
    const exited = session(2);
    const closed = session(3);
    list = [restarted, exited, closed];
    const [cwds, release] = held({ 1: "/a", 2: "/b", 3: "/c" });
    answer({ pty_cwds: cwds, claude_open_sessions: {}, background_tabs: {} });
    const check = poller.poll();
    restarted.starts++;
    exited.exited = true;
    list = [restarted, exited];
    release();
    await check;
    expect([restarted.liveCwd, exited.liveCwd, closed.liveCwd]).toEqual([undefined, undefined, undefined]);
    expect(opts.updateTab).not.toHaveBeenCalled();
    expect(opts.scheduleSave).not.toHaveBeenCalled();
  });

  test("a check that returns after the signal aborted changes nothing", async () => {
    const s = session(1, { finishAfter: 0 });
    list = [s];
    const [cwds, release] = held({ 1: "/a" });
    answer({ pty_cwds: cwds, claude_open_sessions: { 1: { state: "open", session: "a", skipsPermissions: false } }, background_tabs: { t1: 5 } });
    const check = poller.poll();
    lifetime.abort();
    release();
    await check;
    expect([s.liveCwd, s.claudeOpen, s.background, s.finishAfter]).toEqual([undefined, null, null, 0]);
    expect(opts.updateTab).not.toHaveBeenCalled();
    expect(opts.showHistory).not.toHaveBeenCalled();
    expect(opts.scheduleSave).not.toHaveBeenCalled();
    expect(opts.playFinish).not.toHaveBeenCalled();
  });

  test("nothing is checked after the signal aborted", async () => {
    list = [session(1)];
    lifetime.abort();
    await poller.poll();
    expect(ipc).not.toHaveBeenCalled();
  });
});
