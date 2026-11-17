import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { errorText, type ClaudeState, type Layout } from "./ipcTypes";
import type { NoticeSink } from "./notices";
import { claudeStates, liveCwds } from "./tabPoller";
import type { SpawnOptions } from "./tabs";
import type { Session } from "./tabState";

/** Tab changes are saved this long after the last one, so a burst is one save. */
const SAVE_DELAY_MS = 300;
/** A failed save is tried again this long after it failed. */
const SAVE_RETRY_MS = 5_000;

export interface LayoutStoreOptions {
  /** The running app's signal: nothing is saved or restored after it aborts. */
  signal: AbortSignal;
  notices: NoticeSink;
  /** The open tabs, in tab-bar order. */
  tabs(): readonly Session[];
  /** The terminal tab selected last. */
  active(): Session | null;
  /** Notes the Claude session a terminal tab has open, from a check (see `TabPoller.followClaude`). */
  followClaude(s: Session, state: ClaudeState | undefined): void;
  spawn(opts: SpawnOptions): Promise<void>;
  activate(s: Session): void;
  /** Saves the open flow editors' waiting changes; each resolves to whether they are stored. */
  flushEditors(): Promise<boolean[]>;
}

export interface LayoutStore {
  /** Saves the tabs shortly, so a burst of changes is one save; a failed save is retried. */
  schedule(): void;
  /**
   * Saves the open tabs, each in its process's current directory. Does nothing
   * while restoring, or once saving is off (the saved tabs could not be read or
   * fully restored, or `stopSaving` was called).
   */
  save(): Promise<void>;
  /**
   * Reopens the tabs from the last run; with none saved the app starts empty. If the
   * saved tabs cannot be read, or a tab's view cannot be created at all, saving is
   * switched off for this run so the stored layout is kept rather than overwritten.
   * A tab whose process fails to start is not lost: it stays, marked exited, and is
   * saved with its ids.
   */
  restore(): Promise<void>;
  /** Switches saving off for this run, saying why in a notice that stays. */
  stopSaving(message: string): void;
  /**
   * On window close, saves the open flow editors' waiting changes and then the tabs;
   * the handler finishes before the window is destroyed. If the flows' changes are not
   * all stored (or flushing them fails), the first close is cancelled with a warning,
   * and closing again goes on without them. If saving the tabs fails, a close is
   * cancelled with its own warning, and the close after it quits. Resolves to the
   * function that stops watching.
   */
  watchWindowClose(): Promise<() => void>;
  /** Drops a pending save. */
  dispose(): void;
}

/** Saves the open tabs, and reopens them on the next start. */
export function createLayoutStore(opts: LayoutStoreOptions): LayoutStore {
  const { signal, notices } = opts;
  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  // Saving half-restored tabs would lose the rest if the app quit mid-restore.
  let restoring = false;
  // False once the saved layout could not be loaded or fully restored: saving then would overwrite it.
  let canSave = true;
  // What was last stored, so an unchanged layout is not written again.
  let lastSaved = "";

  const savingPaused = () => restoring || !canSave;

  function schedule() {
    if (savingPaused() || signal.aborted) return;
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      if (signal.aborted) return;
      save().catch((err) => {
        console.error("misag: could not save tabs", err);
        if (signal.aborted) return;
        // Tried again, so a passing problem (such as a busy database) does not lose the change.
        saveTimer = setTimeout(schedule, SAVE_RETRY_MS);
      });
    }, SAVE_DELAY_MS);
  }

  async function save() {
    clearTimeout(saveTimer);
    if (savingPaused()) return;
    const sessions = opts.tabs();
    const running = sessions.filter((s) => s.id !== null && !s.exited);
    // Without live directories, each tab keeps the one it was opened in.
    const [cwds, claudes] = await Promise.all([
      liveCwds(running.map((s) => s.id!)),
      claudeStates(running.filter((s) => s.run.kind === "terminal")),
    ]);
    for (const s of sessions) if (s.run.kind === "terminal") opts.followClaude(s, s.id === null ? undefined : claudes[s.id]);
    const active = opts.active();
    const layout: Layout = {
      active: Math.max(0, active ? sessions.indexOf(active) : 0),
      tabs: sessions.map((s) => ({
        ...s.run,
        // A terminal tab is saved with the Claude session open in it, resumed on restore.
        ...(s.run.kind === "terminal" && { claudeSession: s.claudeOpen }),
        tabId: s.tabId,
        name: s.name,
        renamed: s.renamed,
        cwd: (s.id !== null && cwds[s.id]) || s.cwd || null,
      })),
    };
    const json = JSON.stringify(layout);
    if (json === lastSaved) return;
    try {
      await invoke("layout_save", { layout });
      lastSaved = json;
      notices.set("save", null);
    } catch (err) {
      notices.set("save", `Could not save tabs: ${errorText(err)}.`);
      throw err;
    }
  }

  function stopSaving(message: string) {
    canSave = false;
    notices.set("load", message, false);
  }

  async function restore() {
    // Sessions from a previous page load (a dev reload) have no tab any more.
    const reset = invoke("pty_reset").catch((err) => {
      console.error("misag: could not reset sessions", err);
      notices.set("reset", `Leftover sessions could not be stopped (${errorText(err)}); restored Claude tabs may fail to start.`);
    });
    // Loaded while the reset runs; spawning waits for both.
    const load = invoke<Layout>("layout_load");
    let saved: Layout;
    try {
      [, saved] = await Promise.all([reset, load]);
    } catch (err) {
      console.error("misag: could not load saved tabs", err);
      if (signal.aborted) return;
      stopSaving(`Saved tabs could not be read (${errorText(err)}), so changes are not saved this session.`);
      return;
    }
    if (signal.aborted || !saved.tabs.length) return;
    restoring = true;
    let lost = 0;
    try {
      // All at once: each spawn adds its tab before its first await, so the order holds.
      const results = await Promise.allSettled(
        saved.tabs.map(({ tabId, name, renamed, cwd, ...run }) =>
          opts.spawn({ run, tabId, name, renamed, cwd: cwd ?? undefined, fallbackToHome: true }),
        ),
      );
      results.forEach((r, i) => {
        if (r.status !== "rejected") return;
        lost++;
        console.error("misag: could not restore tab", saved.tabs[i].name, r.reason);
      });
    } finally {
      restoring = false;
    }
    if (signal.aborted) return;
    if (lost) stopSaving(`${lost} saved tab(s) could not be reopened, so changes are not saved this session.`);
    const sessions = opts.tabs();
    if (sessions.length) opts.activate(sessions[Math.min(saved.active, sessions.length - 1)]);
  }

  function watchWindowClose(): Promise<() => void> {
    let warned = false;
    let warnedFlows = false;
    return getCurrentWindow().onCloseRequested(async (event) => {
      const flowsSaved = await opts.flushEditors().catch((err) => {
        console.error("misag: could not save the flows' changes", err);
        return [false];
      });
      if (flowsSaved.includes(false) && !warnedFlows) {
        warnedFlows = true;
        event.preventDefault();
        notices.set("flows", "Some flow changes are not saved; their tabs say why. Close the window again to quit without them.", false);
        return;
      }
      try {
        await save();
        warned = false;
      } catch (err) {
        console.error("misag: could not save tabs", err);
        if (warned) return;
        warned = true;
        event.preventDefault();
        notices.set("save", `Could not save tabs: ${errorText(err)}. Close the window again to quit without saving.`, false);
      }
    });
  }

  return { schedule, save, restore, stopSaving, watchWindowClose, dispose: () => clearTimeout(saveTimer) };
}
