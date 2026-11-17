import { invoke } from "@tauri-apps/api/core";
import { errorText, type ClaudeState } from "./ipcTypes";
import { tabTopic, type NoticeSink } from "./notices";
import { confirmFinish, type Session } from "./tabState";

/** How often the tabs' directories and terminal tabs' Claude are checked (see `TabPoller.poll`). */
export const TAB_POLL_MS = 2_000;
/** How long a `claude` must run without reporting before it is taken to bypass the wrapper. */
const UNTRACKED_GRACE_MS = 10_000;
/** How long after a Claude tab starts its session hook should have reported in. */
export const HOOK_REPORT_DELAY_MS = 60_000;
/** How often a Claude tab whose session hook has not reported is checked again. */
const HOOK_RECHECK_MS = 30_000;

/** The live directories of the given sessions; empty if they cannot be read. */
export async function liveCwds(ids: number[]): Promise<Record<number, string>> {
  if (!ids.length) return {};
  return invoke<Record<number, string>>("pty_cwds", { ids }).catch((err) => {
    console.error("misag: could not read tab directories", err);
    return {};
  });
}

/**
 * When (ms) each tab's Claude last ended a turn waiting on background tasks, by tab
 * id; undefined (nothing known) if that cannot be checked, so the tabs keep what
 * they show.
 */
async function backgroundTabs(): Promise<Record<string, number> | undefined> {
  return invoke<Record<string, number>>("background_tabs").catch(
    (err) => void console.error("misag: could not check for background tasks", err),
  );
}

/** Whether each of the given tabs has a Claude session open, by PTY id; empty (nothing known) if that cannot be checked. */
export async function claudeStates(tabs: readonly Session[]): Promise<Record<number, ClaudeState>> {
  if (!tabs.length) return {};
  const query = tabs.map((s) => ({ id: s.id, tabId: s.tabId }));
  return invoke<Record<number, ClaudeState>>("claude_open_sessions", { tabs: query }).catch((err) => {
    console.error("misag: could not check for Claude sessions in terminal tabs", err);
    return {};
  });
}

/**
 * Whether Claude runs in the tab right now, so a prompt typed there goes to Claude
 * and not to a shell. A terminal tab is asked now, not trusted to its last poll.
 */
export async function claudeLive(s: Session): Promise<boolean> {
  if (s.exited || s.closing || s.id === null) return false;
  if (s.run.kind === "claude") return true;
  return (await claudeStates([s]))[s.id]?.state === "open";
}

export interface TabPollerOptions {
  /** The running app's signal: checks and their answers stop changing anything once it aborts. */
  signal: AbortSignal;
  notices: NoticeSink;
  /** The open tabs. */
  tabs(): readonly Session[];
  /** Refreshes the tab's label, tooltip and icon. */
  updateTab(s: Session): void;
  /** Marks again which sessions the tabs have open in the Session history box. */
  showHistory(): void;
  scheduleSave(): void;
  /** Plays the finish sound chosen in the settings. */
  playFinish(): void;
}

export interface TabPoller {
  /**
   * Checks the running tabs' directories, the Claude in terminal tabs, and which
   * tabs' Claude waits on background tasks, so labels and icons follow them. When a
   * directory or a terminal tab's Claude changed it saves (a Claude starting or
   * quitting in a terminal tab changes nothing else) and redraws the Session history
   * box, whose marks of the tabs that have a session open may have changed; a change
   * in what Claude waits on only redraws its tab. A check that read the background
   * tasks also confirms the tabs' due finishes (see `confirmFinish`). Answers for a
   * tab that exited, restarted or closed meanwhile are dropped, and so is a whole
   * check that returns after the signal aborted.
   */
  poll(): Promise<void>;
  /** How many tab checks have started, numbering them for `confirmFinish`. */
  checks(): number;
  /**
   * Updates the Claude session a terminal tab is known to have open from its latest
   * check (`state`, undefined when it was not checked), and its icon and tooltip with
   * it. Only a clear answer changes them: a tab that could not be checked, or never
   * started, keeps what it had. A tab whose shell has exited has no session open,
   * whatever the check says; and a closed answer keeps the session while a resumed
   * Claude may still be starting (until `resumingUntil`).
   */
  followClaude(s: Session, state: ClaudeState | undefined): void;
  /** Reads which session each tab is in; if that fails, the last answer is kept. */
  readTabSessions(): Promise<void>;
  /** The running tab that has Claude session `id` open, if any. */
  tabWithSession(id: string): Session | undefined;
  /**
   * Warns when a Claude tab's session hook has not reported since the tab's start
   * number `startNo` at `since`, so /clear and /resume there would not be remembered.
   * Keeps checking, and clears the warning once the hook reports (e.g. after the
   * user accepts Claude's trust prompt) or the tab goes away.
   */
  watchHookReport(s: Session, since: number, startNo: number): void;
}

/** Follows what runs in the tabs: their directories, their Claude and its sessions, and when it finishes. */
export function createTabPoller(opts: TabPollerOptions): TabPoller {
  const { signal, notices } = opts;
  /** True while `poll` waits for the backend, so a slow check is not started twice. */
  let polling = false;
  let checksStarted = 0;
  /** The Claude session each tab's hook last reported, by tab id, as last read. */
  let reportedSessions: Record<string, string> = {};
  /** The failure last logged by `readTabSessions`, so one that lasts is logged once. */
  let tabSessionsFailure: string | null = null;

  async function poll() {
    if (polling || signal.aborted) return;
    const running = opts.tabs().filter((s) => s.id !== null && !s.exited).map((s) => ({ s, id: s.id!, starts: s.starts }));
    if (!running.length) return;
    polling = true;
    const check = ++checksStarted;
    try {
      const terminals = running.filter(({ s }) => s.run.kind === "terminal");
      const [cwds, claudes, background] = await Promise.all([
        liveCwds(running.map(({ id }) => id)),
        claudeStates(terminals.map(({ s }) => s)),
        backgroundTabs(),
      ]);
      if (signal.aborted) return;
      let changed = false;
      for (const { s, id, starts } of running) {
        if (s.exited || s.starts !== starts || !opts.tabs().includes(s)) continue;
        const before = { dir: s.liveCwd, claude: s.claudeOpen };
        if (s.run.kind === "terminal") followClaude(s, claudes[id]);
        const since = background && (background[s.tabId] ?? null);
        if (since !== undefined && since !== s.background) {
          s.background = since;
          opts.updateTab(s);
        }
        if (background && confirmFinish(s, check)) opts.playFinish();
        if (cwds[id]) s.liveCwd = cwds[id];
        if (s.liveCwd === before.dir && s.claudeOpen === before.claude) continue;
        changed = true;
        opts.updateTab(s);
      }
      if (changed) {
        opts.showHistory();
        opts.scheduleSave();
      }
    } catch (err) {
      console.error("misag: could not check the tabs", err);
    } finally {
      polling = false;
    }
  }

  function followClaude(s: Session, state: ClaudeState | undefined) {
    // Its shell has exited, so nothing can still run in it, whatever a check that
    // started earlier says.
    if (s.id !== null && s.exited) return void (s.claudeOpen = null);
    if (!state || state.state === "unknown") return;
    showClaudeIn(s, state.state !== "closed", state.state !== "closed" && state.skipsPermissions);
    if (state.state === "open") {
      s.claudeOpen = state.session;
      s.resumingUntil = 0;
    } else if (Date.now() >= s.resumingUntil) {
      // Not while a resumed Claude may still be starting.
      s.claudeOpen = null;
    }
    if (state.state !== "untracked") return void (s.untracked = undefined);
    if (s.untracked?.pid !== state.pid) s.untracked = { pid: state.pid, since: Date.now() };
    else if (Date.now() - s.untracked.since >= UNTRACKED_GRACE_MS) {
      notices.set(
        tabTopic(s, "untracked"),
        `Claude in “${s.name}” did not start through Mischief of Agents's claude wrapper (a claude alias, or a PATH that lists another claude first), so it will not be reopened.`,
        true,
        String(state.pid),
      );
    }
  }

  /** Shows whether Claude runs in a terminal tab, and whether it skips permissions, in its icon and tooltip. */
  function showClaudeIn(s: Session, inClaude: boolean, skipsPermissions: boolean) {
    if (inClaude === s.inClaude && skipsPermissions === s.skipsPermissions) return;
    // A Claude that quit mid-turn leaves its spinner in the title.
    if (!inClaude) s.busy = false;
    // A Claude that started or quit is not waiting on what an earlier one waited on.
    if (inClaude !== s.inClaude) {
      s.turnStarted = Date.now();
      s.background = null;
    }
    s.inClaude = inClaude;
    s.skipsPermissions = skipsPermissions;
    opts.updateTab(s);
  }

  async function readTabSessions() {
    try {
      reportedSessions = await invoke<Record<string, string>>("claude_tab_sessions");
      tabSessionsFailure = null;
    } catch (err) {
      if (errorText(err) !== tabSessionsFailure) console.error("misag: could not read the tabs' Claude sessions", err);
      tabSessionsFailure = errorText(err);
    }
  }

  function tabWithSession(id: string): Session | undefined {
    return opts.tabs().find((s) => {
      if (s.exited || s.closing) return false;
      const open = s.run.kind === "claude" ? (reportedSessions[s.tabId] ?? s.run.claudeSession) : s.claudeOpen;
      return open === id;
    });
  }

  function watchHookReport(s: Session, since: number, startNo: number) {
    const topic = tabTopic(s, "hook");
    const check = async () => {
      // A restart has its own watch.
      if (signal.aborted || s.starts !== startNo) return;
      if (s.closing || s.exited || !opts.tabs().includes(s)) return notices.set(topic, null);
      let reported: boolean;
      try {
        reported = await invoke<boolean>("claude_reported", { tabId: s.tabId, since });
      } catch (err) {
        if (signal.aborted) return;
        // Unknown, so neither warn nor clear; try again later.
        console.error("misag: could not check the session report", err);
        return void setTimeout(check, HOOK_RECHECK_MS);
      }
      if (signal.aborted) return;
      if (reported || s.starts !== startNo) return notices.set(topic, null);
      // Keyed by the start, not the text: Claude retitles the tab, which must not undo a dismissal.
      notices.set(
        topic,
        `Claude in “${s.name}” has not reported its session, so /clear and /resume there will not be remembered. ` +
          "Is the folder trusted and are hooks allowed in your Claude settings?",
        true,
        `${since}`,
      );
      setTimeout(check, HOOK_RECHECK_MS);
    };
    setTimeout(check, HOOK_REPORT_DELAY_MS);
  }

  return { poll, checks: () => checksStarted, followClaude, readTabSessions, tabWithSession, watchHookReport };
}
