import { Channel, invoke } from "@tauri-apps/api/core";
import { CLAUDE_ICON, icon } from "./icons";
import { errorText, type ResumeReport, type RunningKind, type Spawned, type TabKind, type TabLaunch } from "./ipcTypes";
import { tabTopic, type Notices } from "./notices";
import type { Pages } from "./pages";
import type { RunController } from "./runController";
import { HOOK_REPORT_DELAY_MS, liveCwds, type TabPoller } from "./tabPoller";
import { BUSY_TITLE, cancelFinish, noteFinish, resetFinish, showsBusy, tabLabel, tabTitle, type Session } from "./tabState";
import { createView, handleMouse } from "./terminalView";
import { DRAG_THRESHOLD, slide } from "./toolbox";

export interface SpawnOptions {
  run: TabKind;
  tabId?: string;
  name?: string;
  renamed?: boolean;
  cwd?: string;
  /** Start in the home directory if `cwd` no longer exists. */
  fallbackToHome?: boolean;
}

export interface TabsOptions {
  /** The running app's signal: timers and continuations stop changing the page once it aborts. */
  signal: AbortSignal;
  /** The tab bar. */
  bar: HTMLElement;
  /** Where the terminals' panes go. */
  terms: HTMLElement;
  notices: Pick<Notices, "set" | "clearTab">;
  runs: Pick<RunController<Session>, "typed" | "locked" | "waiting" | "forgetTab" | "show">;
  pages: Pick<Pages, "shown" | "setShown" | "renderTabs" | "markActive">;
  /** The home directory, shown as `~`; null until known, or if it cannot be. */
  home(): string | null;
  /** How many tab checks have started (see `TabPoller.checks`). */
  checks(): number;
  /** Warns while a started Claude tab's session hook has not reported (see `TabPoller.watchHookReport`). */
  watchHookReport: TabPoller["watchHookReport"];
  scheduleSave(): void;
  /** Marks again which sessions the active tab and the others have open. */
  showHistory(): void;
}

export interface Tabs {
  /** The open terminal tabs, in tab-bar order; the array itself, which they are added to and removed from. */
  readonly list: readonly Session[];
  /** The last terminal tab selected; while a page (such as Settings) is shown, it stays selected underneath. */
  active(): Session | null;
  /** Opens a tab and starts its process. */
  spawn(opts: SpawnOptions): Promise<void>;
  /**
   * Opens a tab of `run` in `chosenCwd`, or else next to the active tab. Only a directory
   * taken from the active tab falls back to $HOME if it has gone; a chosen one fails visibly.
   */
  open(run: TabKind, name?: string, chosenCwd?: string): void;
  /** Selects the tab and shows its terminal. */
  activate(s: Session): void;
  /**
   * Marks the visible tab and pane: the shown page if any, else the active terminal.
   * Classes are toggled in place rather than re-rendering, so a click followed by a
   * double-click (rename) lands on the same tab element.
   */
  showActive(): void;
  /** Shows the active terminal again, the page over it having closed. */
  showTerminal(): void;
  /** Draws the tab bar again: the terminal tabs, then the open pages' tabs. */
  render(): void;
  /**
   * Refreshes one tab's label, tooltip and icon in place; titles, directories and
   * Claude change often. A tab being renamed is left alone, so its input stays open;
   * finishing the rename renders it again.
   */
  updateTab(s: Session): void;
  /** Types into the tab's process. */
  /** Writes to the tab's program; rejects when it does not take it. */
  write(s: Session, data: Uint8Array): Promise<void>;
  /**
   * Whether the person's typing into `s` is held back because a flow is typing its
   * prompt there; says so, so the keys do not seem lost.
   */
  heldBack(s: Session): boolean;
  /** The tab's label, with its folder under the home directory shown as `~`. */
  label(s: Session): string;
  /** Fits the active terminal, unless a page is shown over it. */
  fitShown(): void;
  focusActive(): void;
  /** Drops the finishes the tabs were waiting to confirm. */
  dispose(): void;
}

/** The terminal tabs: their processes, the tab bar, and which one is shown. */
export function createTabs(opts: TabsOptions): Tabs {
  const { signal, bar, notices, runs, pages } = opts;
  const sessions: Session[] = [];
  let active: Session | null = null;
  let counter = 0;
  /** The tab being dragged, if any. */
  let draggedTab: HTMLElement | null = null;
  const encoder = new TextEncoder();

  const label = (s: Session) => tabLabel(s, opts.home());

  async function spawn(spawnOpts: SpawnOptions) {
    const { el, term } = createView(opts.terms);

    const run: RunningKind =
      spawnOpts.run.kind === "claude"
        ? { kind: "claude", claudeSession: spawnOpts.run.claudeSession ?? crypto.randomUUID() }
        : { kind: "terminal", command: spawnOpts.run.command || null, claudeSession: spawnOpts.run.claudeSession ?? null };
    const defaultName =
      run.kind === "claude" ? `Claude ${++counter}` : run.command?.split(" ")[0] || `shell ${++counter}`;
    const s: Session = {
      tabId: spawnOpts.tabId ?? crypto.randomUUID(), run,
      id: null, starts: 0, term, el, exited: false, closing: false, busy: false, background: null, turnStarted: 0, shownBusy: false, finishSettle: null, finishAfter: null, skipsPermissions: false, inClaude: false,
      claudeOpen: run.kind === "terminal" ? run.claudeSession : null, resumingUntil: 0,
      renamed: spawnOpts.renamed ?? Boolean(spawnOpts.name),
      cwd: spawnOpts.cwd || undefined,
      name: spawnOpts.name || defaultName,
    };
    const before = { tab: active, page: pages.shown() };
    sessions.push(s);
    render();
    activate(s);
    try {
      // Output written before this resolves would be dropped.
      await term.init();
    } catch (err) {
      // Closing the tab already cleaned up after it.
      if (s.closing) return;
      sessions.splice(sessions.indexOf(s), 1);
      el.remove();
      render();
      if (active === s) showInstead(before.tab, before.page);
      notices.set("view", `A tab could not be opened: ${errorText(err)}.`);
      throw err;
    }
    // wterm resolves without an error for a view destroyed while it loaded.
    if (s.closing) return;
    // Without autoResize, wterm holds the pane at the height of the rows it started with,
    // so fit() would only ever measure that; the stylesheet sizes the pane instead.
    term.element.style.height = "";
    // The pane must be visible and sized before the PTY is opened with its dimensions.
    term.fit();
    // Focus from `activate` did not reach a terminal that was still loading.
    if (active === s && pages.shown() === null) term.focus();

    term.onData = (data) => {
      if (s.exited) {
        if (data === "\r") restart(s);
        return;
      }
      if (heldBack(s)) return;
      runs.typed(s, data);
      typedInto(s, encoder.encode(data));
    };
    term.onBinary = (data) => {
      if (heldBack(s)) return;
      runs.typed(s, data);
      typedInto(s, data);
    };
    handleMouse(term, { signal, shown: () => active === s && pages.shown() === null, notices });
    term.onResize = (cols, rows) => {
      if (s.id === null || s.exited) return;
      invoke("pty_resize", { id: s.id, cols, rows }).catch((err) =>
        console.error("misag: resize failed", err),
      );
    };
    term.onTitle = (title) => {
      const busy = BUSY_TITLE.test(title);
      if (busy !== s.busy) {
        // A turn that ends unreported (interrupted) leaves an earlier wait on background tasks behind.
        if (busy) s.turnStarted = Date.now();
        s.busy = busy;
        updateTab(s);
      }
      if (!title || s.renamed || title === s.name) return;
      s.name = title;
      updateTab(s);
      opts.scheduleSave();
    };

    await start(s, spawnOpts.fallbackToHome ?? false);
  }

  /**
   * Starts the tab's process. If it fails the tab stays, marked exited with its ids,
   * so saving keeps it for the next restore and Enter can restart it.
   */
  async function start(s: Session, fallbackToHome: boolean) {
    const { term } = s;
    const startNo = ++s.starts;
    // What an earlier Claude in the tab waited on, or was finishing, is not this one's.
    s.turnStarted = Date.now();
    s.background = null;
    resetFinish(s);
    const onData = new Channel<ArrayBuffer>();
    onData.onmessage = (buf) => show(s, new Uint8Array(buf));
    // Attached before pty_spawn runs, so an immediate exit is not missed.
    const onExit = new Channel<number>();
    onExit.onmessage = (code) =>
      markExited(s, code === 0 ? "process exited" : `process exited with code ${code}`);
    // What became of a queued `claude --resume`, which may come before pty_spawn answers.
    let resumed = false;
    const onResume = new Channel<ResumeReport>();
    onResume.onmessage = (report) => {
      if (s.closing || s.starts !== startNo) return;
      resumed = true;
      showResume(s, report);
      opts.scheduleSave();
    };

    const request: TabLaunch = {
      ...s.run,
      tabId: s.tabId,
      cols: term.cols,
      rows: term.rows,
      cwd: s.cwd ?? null,
      fallbackToHome,
    };
    try {
      const started = await invoke<Spawned>("pty_spawn", { request, onData, onExit, onResume });
      // Closed while it started, so close() had no id to kill, and its notices are gone.
      if (s.closing) {
        invoke("pty_kill", { id: started.id }).catch((err) =>
          console.error(`misag: could not kill process ${started.id} of a closed tab`, err),
        );
        return;
      }
      s.id = started.id;
      if (started.fellBackToHome) {
        const message = `${s.cwd} no longer exists, so “${s.name}” started in the home directory.`;
        // Claude redraws its screen at once, so the terminal line alone is easy to miss.
        show(s, `\x1b[2m[${message}]\x1b[0m\r\n`);
        notices.set(tabTopic(s, "fallback"), message);
        s.cwd = undefined;
      }
      if (started.tracked) opts.watchHookReport(s, started.startedAt, startNo);
      if (s.run.kind === "terminal") {
        // Handed over once; from now on saves follow whatever Claude session is open in the tab.
        s.run = { ...s.run, claudeSession: null };
        if (started.resume && !(started.resume.state === "queued" && resumed)) showResume(s, started.resume);
      }
      if (started.skipsPermissions !== s.skipsPermissions) {
        s.skipsPermissions = started.skipsPermissions;
        render();
      }
    } catch (err) {
      // A closed tab has no terminal left to show it on.
      if (s.closing) console.error("misag: could not start the process of a closed tab", err);
      markExited(s, `could not start: ${errorText(err)}`, true);
    }
    opts.scheduleSave();
  }

  /**
   * Follows up a terminal tab's saved Claude session, which its start tried to
   * reopen: pty_spawn's report, then, for a queued one, its onResume's.
   */
  function showResume(s: Session, resume: ResumeReport) {
    const topic = tabTopic(s, "resume");
    if (resume.state === "queued") {
      // Not typed until the shell shows its prompt, so the shell alone does not mean Claude quit.
      s.resumingUntil = Infinity;
      notices.set(topic, null);
    } else if (resume.state === "typed") {
      s.resumingUntil = Date.now() + HOOK_REPORT_DELAY_MS;
      notices.set(topic, null);
    } else if (resume.state === "noConversation") {
      s.claudeOpen = null;
      notices.set(topic, `Claude was open in “${s.name}”, but it saved no conversation there, so it was not reopened.`);
    } else {
      // Kept until a Claude opens in the tab (or its shell exits), so the next start tries again.
      s.resumingUntil = Infinity;
      notices.set(topic, `Claude could not be reopened in “${s.name}” (${resume.message}); the next start tries again.`);
    }
  }

  /** Starts an exited tab again, in the same tab with the same ids (a Claude tab in its current session). */
  function restart(s: Session) {
    if (s.closing || !s.exited) return;
    s.exited = false;
    s.id = null;
    // A full reset (RIS) clears the screen and modes the last process left behind.
    show(s, "\x1bc");
    render();
    start(s, true).catch((err) => console.error("misag: could not restart tab", err));
  }

  /** Writes to a tab's terminal. wterm rethrows what its callbacks threw while it wrote; the output itself is written. */
  function show(s: Session, data: string | Uint8Array) {
    try {
      s.term.write(data);
    } catch (err) {
      console.error("misag: a terminal callback failed", err);
    }
  }

  async function write(s: Session, data: Uint8Array) {
    if (s.id === null || s.exited) return;
    await invoke("pty_write", { id: s.id, data: Array.from(data) });
  }

  /** Writes what the person typed, saying so when the tab does not take it. */
  function typedInto(s: Session, data: Uint8Array) {
    const topic = tabTopic(s, "write");
    write(s, data).then(
      () => notices.set(topic, null),
      (err) => {
        console.error("misag: write failed", err);
        notices.set(topic, `What you typed in ${label(s)} did not reach it: ${errorText(err)}.`);
      },
    );
  }

  function heldBack(s: Session): boolean {
    if (!runs.locked(s)) return false;
    notices.set("flow-typing", `A flow is sending its prompt to ${label(s)}; typing there is paused until it has.`);
    return true;
  }

  function markExited(s: Session, message: string, error = false) {
    if (s.closing) return;
    s.exited = true;
    s.busy = false;
    s.background = null;
    resetFinish(s);
    if (s.run.kind === "terminal") {
      s.inClaude = false;
      s.skipsPermissions = false;
    }
    show(s, `\r\n\x1b[${error ? "31" : "2"}m[${message}; press Enter to restart]\x1b[0m\r\n`);
    render();
  }

  function activate(s: Session) {
    if (active !== s) opts.scheduleSave();
    active = s;
    pages.setShown(null);
    showActive();
    opts.showHistory();
    // At once, so keys typed before the next frame reach it; again once it is fitted.
    s.term.focus();
    requestAnimationFrame(() => {
      s.term.fit();
      s.term.focus();
    });
  }

  function showActive() {
    const shown = pages.shown() !== null;
    sessions.forEach((o, i) => {
      o.el.classList.toggle("active", !shown && o === active);
      // A hidden terminal still takes in its output, but painting it would be wasted work.
      o.term.setRenderingPaused(shown || o !== active);
      bar.children[i]?.classList.toggle("active", !shown && o === active);
    });
    pages.markActive();
    runs.show();
  }

  function showTerminal() {
    if (active) activate(active);
    else showActive();
  }

  /** Shows the given tab or page again (as they were before a tab that could not be opened), or nothing. */
  function showInstead(tab: Session | null, page: ReturnType<Pages["shown"]>) {
    active = null;
    if (!tab || !sessions.includes(tab)) return showActive();
    if (page === null) return activate(tab);
    active = tab;
    pages.setShown(page);
    showActive();
  }

  function close(s: Session) {
    if (s.closing) return;
    s.closing = true;
    cancelFinish(s);
    // Its run can never be answered again: the tab's id is not reused.
    invoke("flow_stop_tab", { tabId: s.tabId }).catch((err) => console.error("misag: could not end the tab's flow run", err));
    // Its run is dropped now, so it is not shown as a run whose tab is gone while the
    // stop is on its way; if the stop fails, the next poll of the runs shows it as one.
    runs.forgetTab(s);
    const i = sessions.indexOf(s);
    sessions.splice(i, 1);
    // A missing session (already exited) is a no-op on the backend.
    if (s.id !== null) {
      invoke("pty_kill", { id: s.id }).catch((err) => console.error("misag: kill failed", err));
    }
    s.term.destroy();
    s.el.remove();
    notices.clearTab(s);

    render();
    if (active === s) {
      const next = sessions[Math.min(i, sessions.length - 1)] ?? null;
      active = null;
      // Behind a shown page, the neighbour is only selected.
      if (next && pages.shown() === null) activate(next);
      else active = next;
    }
    opts.showHistory();
    runs.show();
    opts.scheduleSave();
  }

  function rename(s: Session, tab: HTMLElement) {
    const input = document.createElement("input");
    // What the tab shows; leaving it unchanged keeps the tab following its folder.
    const shown = label(s);
    input.value = shown;
    tab.replaceChildren(input);
    input.focus();
    input.select();
    // Re-rendering removes the input, which fires blur; only the first outcome counts.
    let finished = false;
    const done = (save: boolean) => {
      if (finished) return;
      finished = true;
      const name = input.value.trim();
      if (save && name && name !== shown) {
        s.name = name;
        s.renamed = true;
        opts.scheduleSave();
      }
      render();
    };
    input.onkeydown = (e) => {
      if (e.key === "Enter") done(true);
      if (e.key === "Escape") done(false);
    };
    input.onblur = () => done(true);
    input.onclick = (e) => e.stopPropagation();
  }

  function render() {
    document.body.classList.toggle("no-tabs", !sessions.length);
    bar.replaceChildren(
      ...sessions.map((s) => {
        const tab = document.createElement("div");
        tab.className = "tab";
        tab.title = tabTitle(s);
        tab.classList.toggle("active", s === active && pages.shown() === null);
        tab.classList.toggle("exited", s.exited);
        tab.classList.toggle("busy", showsBusy(s));

        const name = document.createElement("span");
        name.className = "name";
        name.textContent = label(s);
        tab.classList.toggle("needs-you", runs.waiting(s));

        const x = document.createElement("button");
        x.className = "x";
        x.append(icon("x", 12));
        x.title = "Close tab";
        x.onclick = (e) => {
          e.stopPropagation();
          close(s);
        };

        tab.append(tabIcon(s), name, x);
        tab.onclick = () => activate(s);
        tab.ondblclick = () => rename(s, tab);
        tab.onmousedown = (e) => {
          if (e.button !== 0 || (e.target instanceof Element && e.target.closest(".x, input"))) return;
          // Keeps the tab from taking focus on press, so the terminal keeps it until activate moves it.
          e.preventDefault();
          dragTab(s, e);
        };
        // Middle-click closes the tab.
        tab.onauxclick = (e) => {
          if (e.button === 1) close(s);
        };
        return tab;
      }),
    );
    pages.renderTabs(bar);
  }

  /** The terminal tab of `s` in the tab bar, if it is still open. */
  const tabOf = (s: Session) => bar.children[sessions.indexOf(s)] as HTMLElement | undefined;

  /** Moves the tab of `s` to place `to` among the terminal tabs, sliding the tab it passes into its new place. */
  function moveTab(s: Session, to: number) {
    const from = sessions.indexOf(s);
    const tab = tabOf(s)!;
    const passed = bar.children[to] as HTMLElement;
    const before = passed.offsetLeft;
    for (const a of passed.getAnimations?.() ?? []) a.cancel();
    sessions.splice(from, 1);
    sessions.splice(to, 0, s);
    if (to < from) passed.before(tab);
    else passed.after(tab);
    slide(passed, before - passed.offsetLeft, "X");
  }

  /**
   * Drags the tab of `s` along the tab bar once the mouse has moved far enough, among
   * the terminal tabs only (pages stay after them). The new order is saved when it is let go.
   */
  function dragTab(s: Session, down: MouseEvent) {
    const startTab = tabOf(s)!;
    /** Where in the tab it was grabbed, so it stays under the mouse at that point. */
    const grab = down.clientX - startTab.offsetLeft;
    let tab: HTMLElement | null = null;
    /** How far the dragged tab is drawn off its place, to follow the mouse. */
    let shift = 0;

    const move = (e: MouseEvent) => {
      if (!tab && Math.abs(e.clientX - down.clientX) < DRAG_THRESHOLD) return;
      // Re-rendered or closed meanwhile: the drag ends where it is.
      if (startTab !== tabOf(s)) return up();
      if (!tab) {
        tab = draggedTab = startTab;
        for (const a of tab.getAnimations?.() ?? []) a.cancel();
        tab.classList.add("dragging");
        document.body.classList.add("dragging-tab");
      }
      e.preventDefault();
      const left = e.clientX - grab;
      for (;;) {
        const i = sessions.indexOf(s);
        const prev = bar.children[i - 1] as HTMLElement | undefined;
        const next = i + 1 < sessions.length ? (bar.children[i + 1] as HTMLElement) : undefined;
        if (prev && left < prev.offsetLeft + prev.offsetWidth / 2) moveTab(s, i - 1);
        else if (next && left + tab.offsetWidth > next.offsetLeft + next.offsetWidth / 2) moveTab(s, i + 1);
        else break;
      }
      shift = left - tab.offsetLeft;
      tab.style.transform = `translateX(${shift}px)`;
    };

    const up = () => {
      document.removeEventListener("mousemove", move);
      document.removeEventListener("mouseup", up);
      if (!tab) return;
      const dropped = tab;
      draggedTab = null;
      // Settles into its place from where it was let go.
      dropped.style.transform = "";
      document.body.classList.remove("dragging-tab");
      const settle = slide(dropped, shift, "X");
      // Not when it was grabbed again meanwhile, which cancels this animation.
      const done = () => draggedTab !== dropped && dropped.classList.remove("dragging");
      if (settle) settle.onfinish = settle.oncancel = done;
      else done();
      opts.scheduleSave();
    };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
  }

  /** The tab's icon: a loader while Claude works in it, Claude's while Claude runs in it, else a terminal. */
  function tabIcon(s: Session): HTMLElement {
    const mark = document.createElement("span");
    mark.className = "icon";
    const claude = s.run.kind === "claude" || s.inClaude;
    mark.dataset.kind = showsBusy(s) ? "loader" : claude ? "claude" : "terminal";
    if (showsBusy(s)) mark.append(icon("loader-circle"));
    else if (claude) mark.innerHTML = CLAUDE_ICON;
    else mark.append(icon("square-terminal"));
    return mark;
  }

  function updateTab(s: Session) {
    // Before the elements are looked up: a tab being renamed has none, and still finishes.
    noteFinish(s, () => (signal.aborted ? null : opts.checks()));
    const tab = bar.children[sessions.indexOf(s)];
    const name = tab?.querySelector(".name");
    const mark = tab?.querySelector(".icon");
    if (!(tab instanceof HTMLElement) || !name || !mark) return;
    tab.title = tabTitle(s);
    tab.classList.toggle("busy", showsBusy(s));
    name.textContent = label(s);
    mark.replaceWith(tabIcon(s));
  }

  /** The active tab's current directory, so a new tab opens next to it. */
  async function activeCwd(): Promise<string | undefined> {
    if (!active) return undefined;
    const live = active.id !== null && !active.exited ? (await liveCwds([active.id]))[active.id] : undefined;
    return live ?? active.cwd;
  }

  function open(run: TabKind, name?: string, chosenCwd?: string) {
    (async () => {
      const cwd = chosenCwd || (await activeCwd());
      await spawn({ run, name, cwd, fallbackToHome: !chosenCwd });
    })().catch((err) => console.error(`misag: could not open ${run.kind} tab`, err));
  }

  return {
    list: sessions,
    active: () => active,
    spawn,
    open,
    activate,
    showActive,
    showTerminal,
    render,
    updateTab,
    write,
    heldBack,
    label,
    fitShown: () => {
      if (pages.shown() === null) active?.term.fit();
    },
    focusActive: () => active?.term.focus(),
    dispose: () => sessions.forEach(cancelFinish),
  };
}
