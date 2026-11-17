import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
  BUSY_TITLE,
  cancelFinish,
  claudeHere,
  claudeWorking,
  confirmFinish,
  FINISH_SETTLE_MS,
  noteFinish,
  resetFinish,
  showsBusy,
  tabLabel,
  tabTitle,
  waitsInBackground,
  type TabState,
} from "./tabState";

type Tab = TabState & { name: string; renamed: boolean; cwd?: string; liveCwd?: string; skipsPermissions: boolean };

/** An idle Claude tab whose latest turn began at 100 ms. */
function tab(over: Partial<Tab> = {}): Tab {
  return {
    run: { kind: "claude", claudeSession: "s" },
    exited: false, busy: false, background: null, turnStarted: 100, inClaude: false,
    shownBusy: false, finishSettle: null, finishAfter: null,
    name: "Claude 1", renamed: false, skipsPermissions: false,
    ...over,
  };
}
const shell = (over: Partial<Tab> = {}) => tab({ run: { kind: "terminal", command: null, claudeSession: null }, ...over });

describe("whether Claude works in a tab", () => {
  test("a Claude tab has Claude until it exits; a terminal tab only while Claude is in its foreground", () => {
    expect(claudeHere(tab())).toBe(true);
    expect(claudeHere(tab({ exited: true }))).toBe(false);
    expect(claudeHere(shell())).toBe(false);
    expect(claudeHere(shell({ inClaude: true }))).toBe(true);
    expect(claudeHere(shell({ inClaude: true, exited: true }))).toBe(false);
  });

  test("a spinner in the title counts only where Claude runs", () => {
    expect(claudeWorking(tab({ busy: true }))).toBe(true);
    expect(claudeWorking(shell({ busy: true }))).toBe(false);
    expect(claudeWorking(shell({ busy: true, inClaude: true }))).toBe(true);
    expect(claudeWorking(tab({ busy: true, exited: true }))).toBe(false);
  });

  test("background tasks count when reported since the latest turn began, and only where Claude runs", () => {
    expect(waitsInBackground(tab())).toBe(false);
    expect(waitsInBackground(tab({ background: 100 }))).toBe(true);
    expect(waitsInBackground(tab({ background: 99 }))).toBe(false);
    expect(waitsInBackground(shell({ background: 200 }))).toBe(false);
    expect(waitsInBackground(tab({ background: 200, exited: true }))).toBe(false);
  });

  test("a tab shows busy while Claude works or waits on background tasks", () => {
    expect(showsBusy(tab())).toBe(false);
    expect(showsBusy(tab({ busy: true }))).toBe(true);
    expect(showsBusy(tab({ background: 150 }))).toBe(true);
    expect(showsBusy(shell({ busy: true, background: 150 }))).toBe(false);
  });

  test("Claude's spinner frames mark a busy title, its idle mark does not", () => {
    expect(BUSY_TITLE.test("◐ Fixing tests")).toBe(true);
    expect(BUSY_TITLE.test("⠂ Fixing tests")).toBe(true);
    expect(BUSY_TITLE.test("✳ Fixing tests")).toBe(false);
    expect(BUSY_TITLE.test("◐Fixing")).toBe(false);
  });
});

describe("a tab's label and tooltip", () => {
  test("the label is the user's name, else the live or opened folder (home as ~), else the title", () => {
    expect(tabLabel(tab({ name: "title" }), "/home/u")).toBe("title");
    expect(tabLabel(tab({ cwd: "/home/u" }), "/home/u")).toBe("~");
    expect(tabLabel(tab({ cwd: "/home/u/work" }), "/home/u")).toBe("work");
    expect(tabLabel(tab({ cwd: "/home/u/old", liveCwd: "/home/u/work" }), "/home/u")).toBe("work");
    expect(tabLabel(tab({ cwd: "/home/u/work", name: "build", renamed: true }), "/home/u")).toBe("build");
  });

  test("the tooltip adds the folder, background waiting when idle, and skipped permissions", () => {
    expect(tabTitle(tab({ cwd: "/w" }))).toBe("Claude 1\n/w");
    expect(tabTitle(tab({ background: 150 }))).toBe("Claude 1\n(Claude is waiting on background tasks)");
    expect(tabTitle(tab({ background: 150, busy: true }))).toBe("Claude 1");
    expect(tabTitle(tab({ skipsPermissions: true }))).toBe(
      "Claude 1\n(Claude runs tools without asking: --dangerously-skip-permissions)",
    );
  });
});

describe("the finish sound's rules", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  /** Notes the tab as it is now; a settled finish becomes due from check `check`. */
  const note = (s: Tab, check: number | null = 7) => noteFinish(s, () => check);

  test("a turn that ends is due once it has stayed idle for the settle time", () => {
    const s = tab({ busy: true });
    note(s);
    expect(s.shownBusy).toBe(true);
    s.busy = false;
    note(s);
    expect(s.finishSettle).not.toBeNull();
    vi.advanceTimersByTime(FINISH_SETTLE_MS - 1);
    expect(s.finishAfter).toBeNull();
    vi.advanceTimersByTime(1);
    expect(s.finishSettle).toBeNull();
    expect(s.finishAfter).toBe(7);
  });

  test("nothing is due for a tab that never showed busy, nor when the settle gives no check", () => {
    const idle = tab();
    note(idle);
    expect(idle.finishSettle).toBeNull();
    const stopped = tab({ shownBusy: true });
    note(stopped, null);
    vi.advanceTimersByTime(FINISH_SETTLE_MS);
    expect(stopped.finishAfter).toBeNull();
  });

  test("working again before it settles, or after, drops the finish", () => {
    const s = tab({ shownBusy: true });
    note(s);
    s.busy = true;
    note(s);
    expect(s.finishSettle).toBeNull();
    vi.advanceTimersByTime(FINISH_SETTLE_MS);
    expect(s.finishAfter).toBeNull();
    s.busy = false;
    note(s);
    vi.advanceTimersByTime(FINISH_SETTLE_MS);
    expect(s.finishAfter).toBe(7);
    s.background = 150;
    note(s);
    expect(s.finishAfter).toBeNull();
  });

  test("noting an idle tab again does not restart a settle already under way", () => {
    const s = tab({ shownBusy: true });
    note(s);
    const timer = s.finishSettle;
    vi.advanceTimersByTime(FINISH_SETTLE_MS / 2);
    note(s);
    expect(s.finishSettle).toBe(timer);
  });

  test("only a check started after the due one confirms, and only while Claude is idle and still there", () => {
    expect(confirmFinish(tab(), 9)).toBe(false);
    const s = tab({ finishAfter: 7 });
    expect(confirmFinish(s, 7)).toBe(false);
    expect(s.finishAfter).toBe(7);
    expect(confirmFinish(s, 8)).toBe(true);
    expect(s.finishAfter).toBeNull();
  });

  test("a turn that ends waiting on background tasks, or a Claude that exits or quits, has not finished", () => {
    for (const s of [tab({ finishAfter: 1, background: 150 }), tab({ finishAfter: 1, exited: true }), shell({ finishAfter: 1 })]) {
      expect(confirmFinish(s, 2)).toBe(false);
      // Settled either way, so a later check does not play it.
      expect(s.finishAfter).toBeNull();
    }
  });

  test("cancelling clears the settle timer, and a reset also forgets the tab showed busy", () => {
    const s = tab({ shownBusy: true });
    note(s);
    cancelFinish(s);
    expect(vi.getTimerCount()).toBe(0);
    expect(s.shownBusy).toBe(false);
    s.shownBusy = true;
    s.finishAfter = 3;
    resetFinish(s);
    expect(s).toMatchObject({ shownBusy: false, finishAfter: null, finishSettle: null });
  });
});
