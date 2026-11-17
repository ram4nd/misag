// A terminal tab's state, and the rules read from it: what the tab shows, and when Claude in it has finished.
import type { WTerm } from "@wterm/dom";
import type { RunningKind } from "./ipcTypes";
import { folderLabel } from "./paths";

export interface Session {
  /** Stable across restarts; a Claude tab's session hook reports under this id. */
  tabId: string;
  /**
   * For Claude tabs, `claudeSession` is the session the tab was launched with;
   * later changes (`/clear`, `/resume`) are tracked by the backend's session hook.
   */
  run: RunningKind;
  /** Backend PTY id; null until pty_spawn resolves, and after it failed. */
  id: number | null;
  /** Counts the tab's starts (restarts included), so checks from an earlier start can tell they are stale. */
  starts: number;
  /** The user's name for the tab, or else the terminal's latest title (shown in the tooltip). */
  name: string;
  /** True once the user renamed the tab, so terminal titles stop overwriting it. */
  renamed: boolean;
  /** The terminal title starts with a spinner frame: Claude is working, if it runs in the tab (see `claudeWorking`). */
  busy: boolean;
  /** When (ms) the tab's Claude last ended a turn with background tasks in flight, which wake it again; null if not (see `waitsInBackground`). */
  background: number | null;
  /** When (ms) the latest turn of the tab's Claude began, or this Claude started: a `background` report from before it is stale. */
  turnStarted: number;
  /** Whether the tab showed Claude working when last updated (see `noteFinish`). */
  shownBusy: boolean;
  /** Sets `finishAfter` once Claude has stayed idle for `FINISH_SETTLE_MS`; null when not waiting. */
  finishSettle: ReturnType<typeof setTimeout> | null;
  /** A finish is due: the first tab check numbered higher than this confirms it (see `confirmFinish`); null when none is. */
  finishAfter: number | null;
  /** Directory the tab was opened in; saves use its live directory when they can. */
  cwd?: string;
  /** The process's current directory, last time it was checked. */
  liveCwd?: string;
  term: WTerm;
  el: HTMLDivElement;
  exited: boolean;
  closing: boolean;
  /** The tab's Claude was started with `--dangerously-skip-permissions`. */
  skipsPermissions: boolean;
  /** For terminal tabs: Claude was in the foreground at the latest clear check. */
  inClaude: boolean;
  /**
   * For terminal tabs: the Claude session last known to be open in the shell,
   * saved with the tab so a restore reopens it. A check that cannot tell keeps it.
   */
  claudeOpen: string | null;
  /**
   * Until when (ms) a resumed Claude may still be starting, so a shell without it does not mean it quit:
   * Infinity while its `claude --resume` waits for the shell's prompt, and after a resume that failed.
   */
  resumingUntil: number;
  /** A `claude` seen in the foreground that never reported, and since when (ms). */
  untracked?: { pid: number; since: number };
}

/** The parts of a tab that the rules in this module (`showsBusy`, `noteFinish`, `confirmFinish` and the rest) read and change. */
export type TabState = Pick<
  Session,
  "run" | "exited" | "busy" | "background" | "turnStarted" | "inClaude" | "shownBusy" | "finishSettle" | "finishAfter"
>;

/**
 * How long Claude must stay idle before a tab check can confirm it finished: time
 * for its Stop hook to record background tasks, which a check started earlier
 * could miss. The sound then plays on the next check, up to a check interval (TAB_POLL_MS in tabPoller.ts) later.
 */
export const FINISH_SETTLE_MS = 1_000;

/** Claude titles the terminal `✳ <task>` while idle, and with a spinner frame (`◐ <task>`, `⠂ <task>`) while working. */
export const BUSY_TITLE = /^[◐-◓⠁-⣿] /;

/** The tab's directory: its live one, else the one it was opened in. */
export const tabDir = (s: Pick<Session, "liveCwd" | "cwd">) => s.liveCwd ?? s.cwd;

/** The tab's label: the user's name for it, else its folder (under `home`, shown as `~`), else its title until the folder is known. */
export function tabLabel(s: Pick<Session, "liveCwd" | "cwd" | "renamed" | "name">, home: string | null): string {
  const dir = tabDir(s);
  return s.renamed || !dir ? s.name : folderLabel(dir, home);
}

/**
 * The tab's tooltip: its title and directory, whether Claude only waits on
 * background tasks, and a warning while Claude runs tools without asking.
 */
export function tabTitle(s: TabState & Pick<Session, "liveCwd" | "cwd" | "name" | "skipsPermissions">): string {
  const dir = tabDir(s);
  let text = dir ? `${s.name}\n${dir}` : s.name;
  if (waitsInBackground(s) && !claudeWorking(s)) text += "\n(Claude is waiting on background tasks)";
  return s.skipsPermissions ? `${text}\n(Claude runs tools without asking: --dangerously-skip-permissions)` : text;
}

/** Whether Claude runs in the tab now. */
export function claudeHere(s: TabState): boolean {
  return !s.exited && (s.run.kind === "claude" || s.inClaude);
}

/** Whether Claude is mid-turn in the tab: another tool's spinner in a plain shell does not count. */
export function claudeWorking(s: TabState): boolean {
  return s.busy && claudeHere(s);
}

/** Whether Claude in the tab ended its latest turn waiting on background tasks. */
export function waitsInBackground(s: TabState): boolean {
  return s.background !== null && s.background >= s.turnStarted && claudeHere(s);
}

/** Whether the tab shows Claude working: mid-turn, or waiting on its background tasks. */
export function showsBusy(s: TabState): boolean {
  return claudeWorking(s) || waitsInBackground(s);
}

/**
 * Notes when the tab stops showing Claude working: once it has stayed so for
 * `FINISH_SETTLE_MS`, a finish is due from the check number `settled` gives (null
 * leaves none due), which a later tab check confirms (see `confirmFinish`).
 * Working again drops it.
 */
export function noteFinish(s: TabState, settled: () => number | null) {
  const busy = showsBusy(s);
  if (busy) cancelFinish(s);
  else if (s.shownBusy && s.finishSettle === null && s.finishAfter === null) {
    s.finishSettle = setTimeout(() => {
      s.finishSettle = null;
      const check = settled();
      if (check !== null) s.finishAfter = check;
    }, FINISH_SETTLE_MS);
  }
  s.shownBusy = busy;
}

/** Drops a finish the tab was waiting to confirm. */
export function cancelFinish(s: Pick<Session, "finishSettle" | "finishAfter">) {
  if (s.finishSettle !== null) clearTimeout(s.finishSettle);
  s.finishSettle = null;
  s.finishAfter = null;
}

/** Forgets what the tab showed: its process exited or started, so no turn of it is finishing. */
export function resetFinish(s: Pick<Session, "finishSettle" | "finishAfter" | "shownBusy">) {
  cancelFinish(s);
  s.shownBusy = false;
}

/**
 * Whether a due finish is confirmed by tab check `check`, which started after the
 * finish settled and read the background tasks: true (play the finish sound) when
 * it shows Claude still idle and still in the tab, as a turn that ends waiting on
 * background tasks, or a Claude that exits or quits, has not finished. A check
 * past the due one settles it either way.
 */
export function confirmFinish(s: TabState, check: number): boolean {
  if (s.finishAfter === null || check <= s.finishAfter) return false;
  const finished = !showsBusy(s) && claudeHere(s);
  cancelFinish(s);
  return finished;
}
