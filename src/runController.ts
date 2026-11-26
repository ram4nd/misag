import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import type { Flow, Flows, Run } from "./flows";

/** Mirrors `runs::Started` in src-tauri/src/runs.rs. */
export interface Started {
  runId: number;
  clear: boolean;
  prompt: string | null;
}

/** Mirrors `runs::StepPrompt`: a step's prompt to type, and the step the run is now at. */
export interface StepPrompt {
  stepId: string;
  prompt: string;
  /** Whether the prompt carries the run's rules, which Claude lacks. */
  briefs: boolean;
}

/** Mirrors `runs::Prefill`. */
export interface Prefill {
  text: string;
  token: number;
}

/** Mirrors `runs::TabTitle`: a title Claude wrote for a tab at a flow's tab title step. */
export interface TabTitle {
  tabId: string;
  title: string;
}

/** Mirrors `runs::Cleared`. */
export type Cleared = { state: "pending" | "gone" } | { state: "ready"; prompt: string | null };

/** How long after pasting a prompt Claude gets the Enter that sends it, so the paste is not taken as typing. */
const PASTE_SUBMIT_MS = 150;
/**
 * For a flow whose Start clears: how often to ask whether Claude has reported the
 * clear (the SessionStart hook does), and how long to wait for it before the run
 * is stopped. The backend makes a run still at Start a little later wait for the
 * person (`runs::CLEAR_TIMEOUT_SECS`), for when this page is not there to stop it.
 */
const CLEAR_POLL_MS = 250;
/** How long Claude gets to clear its input (Ctrl+C) before a flow's prompt is pasted. */
const CLEAR_INPUT_MS = 100;
const CLEAR_TIMEOUT_MS = 30_000;

const encoder = new TextEncoder();
const decoder = new TextDecoder();
/**
 * What the terminal sends on its own rather than the person typing: focus reports,
 * replies to queries (OSC and DCS strings; device attributes, cursor position, status,
 * color scheme, mode, window size and kitty keyboard flag reports) and mouse reports.
 * Kitty key events (CSI u without `?`) are typing, so they stay.
 */
const TERMINAL_REPORTS =
  /\x1b\[[IO]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1bP[^\x1b]*\x1b\\|\x1b\[[?>=][\d;]*[cnu]|\x1b\[\??[\d;]*\$y|\x1b\[\d+;\d+R|\x1b\[\d*n|\x1b\[\d+(?:;\d+)*t|\x1b\[<\d+;\d+;\d+[Mm]|\x1b\[\d+;\d+;\d+M|\x1b\[M[\s\S]{3}/gu;

/** A tab, as flow runs need it. */
export interface RunTab {
  tabId: string;
}

/** What a flow editor shows of a run. */
export interface RunEditor {
  showRun(run: Run | null): void;
  /** Saves its pending changes; resolves once they are stored (or failed, which it shows). */
  flush(): Promise<unknown>;
}

export interface RunControllerOptions<T extends RunTab> {
  flows: Pick<Flows, "showRuns" | "showBoxError" | "reload">;
  errorText(err: unknown): string;
  /** The open tabs, in tab-bar order. */
  tabs(): readonly T[];
  /** The tab selected last. */
  active(): T | null;
  /** The tab's name, for messages. */
  label(tab: T): string;
  /** Whether Claude is working in the tab. */
  busy(tab: T): boolean;
  /**
   * Whether Claude in the tab is idle but waiting on background tasks, which wake it
   * for another turn when they finish.
   */
  waitsInBackground(tab: T): boolean;
  /**
   * Whether Claude runs in the tab right now, so a prompt typed there goes to Claude
   * and not to a shell.
   */
  live(tab: T): Promise<boolean>;
  /** Types into the tab's terminal. */
  /** Writes to the tab; rejects when the tab does not take it. */
  write(tab: T, data: Uint8Array): Promise<void> | void;
  /** The open flow editors, by flow id. */
  editors(): Iterable<[number, RunEditor]>;
  /** Marks in the tab bar the tabs whose run waits for the person. */
  markWaiting(isWaiting: (tab: T) => boolean): void;
  /** Names the tab `title`, which Claude wrote at a flow's tab title step. */
  retitle(tab: T, title: string): void;
  /** The tab's keyboard was locked (a flow is typing there) or unlocked again. */
  lockChanged?(tab: T, locked: boolean): void;
}

export interface RunController<T extends RunTab> {
  /** Starts the flow in the active tab (▶). */
  start(flow: Flow): Promise<void>;
  stop(run: Run): Promise<void>;
  /** Goes on as `output` from the step the run was shown at; resolves to why it could not, or null. */
  proceed(run: Run, output: string): Promise<string | null>;
  /** Has Claude go on with the step that needs attention; resolves to why it could not, or null. */
  resume(run: Run): Promise<string | null>;
  /** Reads the runs again and shows them. */
  poll(): Promise<void>;
  /** Shows the active tab's runs again, as the tab or page shown changed. */
  show(): void;
  /** Forgets the closing tab's run until the next poll. */
  forgetTab(tab: T): void;
  /** Whether the tab's run waits for the person. */
  waiting(tab: T): boolean;
  /** The saved tabs are back, so a run whose tab is not open has lost it. */
  restored(): void;
  /** Notes what the person typed into the tab, so a flow knows whether to clear Claude's input first. */
  typed(tab: T, data: string | Uint8Array): void;
  /** Whether the person's typing into the tab is held back while a flow types there. */
  locked(tab: T): boolean;
}

/** Starts, moves on, stops and shows the flow runs of the app's tabs. */
export function createRunController<T extends RunTab>(opts: RunControllerOptions<T>): RunController<T> {
  const { flows } = opts;
  /** Tabs a flow is being started in (through its clear), so ▶ is not taken twice. */
  const startingTabs = new Set<string>();
  /** Tabs whose flow run waits for the person, marked in the tab bar. */
  const waitingTabs = new Set<string>();
  /** The active runs, as last read. */
  let runs: Run[] = [];
  /** Until the saved tabs are back, a run's tab may just not be open yet. */
  let tabsRestored = false;
  /** How many polls were sent, and the number of the latest one whose reply was shown. */
  let polls = 0;
  let shownPoll = 0;
  /** The error poll shows, so a later good poll clears only its own. */
  let runsError: string | null = null;
  /** Tabs whose Claude input may hold text the person typed and has not sent. */
  const dirtyTabs = new Set<string>();
  /** Tabs whose last input ended in a backslash, which turns an Enter that follows on its own into a newline. */
  const backslashTabs = new Set<string>();
  /** Runs whose manual prompt is being put into Claude's input right now. */
  const filling = new Set<number>();
  /** Runs whose manual prompt could not be put into Claude's input, or waits for the person to clear it, said once. */
  const fillFailed = new Set<number>();
  /** Tabs whose Claude input holds a manual prompt's paste the person has not touched. */
  const pastedTabs = new Set<string>();
  /** How many flow starts and prompts are typing into each tab right now. */
  const lockCounts = new Map<string, number>();

  /**
   * Holds back the person's typing in `tab` while `work` runs, so it cannot mix
   * with what a flow types there; nested locks count, the last one releases.
   */
  async function withLock<R>(tab: T, work: () => Promise<R>): Promise<R> {
    const count = lockCounts.get(tab.tabId) ?? 0;
    lockCounts.set(tab.tabId, count + 1);
    if (count === 0) opts.lockChanged?.(tab, true);
    try {
      return await work();
    } finally {
      const left = (lockCounts.get(tab.tabId) ?? 1) - 1;
      if (left > 0) {
        lockCounts.set(tab.tabId, left);
      } else {
        lockCounts.delete(tab.tabId);
        opts.lockChanged?.(tab, false);
      }
    }
  }

  /**
   * Shows the active tab's runs, their flows marked in the toolbox and their steps in
   * the editors, and in the toolbox the runs whose tab is gone, so they can be stopped.
   */
  function show() {
    const active = opts.active();
    const here = runs.filter((r) => r.tabId === active?.tabId);
    const orphans = tabsRestored ? runs.filter((r) => !opts.tabs().some((t) => t.tabId === r.tabId)) : [];
    flows.showRuns(here, orphans);
    for (const [flowId, editor] of opts.editors()) editor.showRun(here.find((r) => r.flowId === flowId) ?? null);
  }

  /**
   * Types `text` into the tab as one paste and sends it, as if the person had. The
   * backend drops control characters from prompts, so the text cannot end the paste.
   * If Claude is busy (mid-turn, or in a menu), the paste lands there instead. Claude
   * is checked before the paste and again before the Enter: in a terminal tab it may
   * have been suspended or quit meanwhile, and the shell must not run a flow's text.
   * Resolves to whether it was sent; false if Claude had left, when it was not.
   */
  function typePrompt(tab: T, text: string): Promise<boolean> {
    return withLock(tab, async () => {
      const left = () => {
        console.warn(`misag: Claude left ${opts.label(tab)} before its flow prompt was sent; it was not sent`);
        return false;
      };
      if (!(await opts.live(tab))) return left();
      // Text the person left in Claude's input would be sent along with the prompt.
      // Ctrl+C clears it; on an empty input one press only shows a hint, so it is
      // sent only when the person typed since their last Enter.
      // The input is about to be cleared or sent, so a backslash the person ended on is gone.
      backslashTabs.delete(tab.tabId);
      if (dirtyTabs.delete(tab.tabId)) {
        await opts.write(tab, encoder.encode("\x03"));
        await new Promise((resolve) => setTimeout(resolve, CLEAR_INPUT_MS));
      }
      await opts.write(tab, encoder.encode(`\x1b[200~${text}\x1b[201~`));
      await new Promise((resolve) => setTimeout(resolve, PASTE_SUBMIT_MS));
      if (!(await opts.live(tab))) {
        // The paste may wait in the input of a Claude brought back (fg): cleared before the next prompt.
        dirtyTabs.add(tab.tabId);
        return left();
      }
      await opts.write(tab, encoder.encode("\r"));
      return true;
    });
  }

  /**
   * Puts the text of the manual prompt `run` waits at into its tab's Claude input,
   * as a paste without Enter, for the person to finish and send. Waits while Claude
   * works or a flow types there, and while the input holds text the person typed,
   * which it does not wipe: it says how to get the draft, once. The backend owes
   * the paste until it is told it was made, so a paste that fails is tried again
   * at the next poll; its error is shown once.
   */
  async function fill(run: Run) {
    const tab = opts.tabs().find((t) => t.tabId === run.tabId);
    if (!tab || filling.has(run.id) || lockCounts.has(tab.tabId) || opts.busy(tab)) return;
    if (dirtyTabs.has(tab.tabId)) {
      if (!fillFailed.has(run.id)) {
        fillFailed.add(run.id);
        flows.showBoxError(
          `Claude's input in ${opts.label(tab)} has text in it: clear it (Ctrl+C) or send it, and "${run.step}"'s draft goes in.`,
        );
      }
      return;
    }
    filling.add(run.id);
    let pasted = false;
    try {
      await withLock(tab, async () => {
        if (!(await opts.live(tab))) return;
        const fill = await invoke<Prefill | null>("flow_prefill_text", { runId: run.id, stepId: run.stepId });
        // Claude may have started a turn, or the person typed, meanwhile.
        if (fill === null || opts.busy(tab) || dirtyTabs.has(tab.tabId)) return;
        await opts.write(tab, encoder.encode(`\x1b[200~${fill.text}\x1b[201~`));
        pasted = true;
        dirtyTabs.add(tab.tabId);
        pastedTabs.add(tab.tabId);
        await invoke("flow_prefilled", { runId: run.id, stepId: run.stepId, token: fill.token });
      });
      fillFailed.delete(run.id);
    } catch (err) {
      console.error("misag: could not fill Claude's input for the manual prompt", err);
      if (!fillFailed.has(run.id)) {
        fillFailed.add(run.id);
        flows.showBoxError(
          pasted
            ? `Pasted "${run.step}" into ${opts.label(tab)}, but could not record it (${opts.errorText(err)}); it is pasted again once the input is cleared.`
            : `Could not put the manual prompt into ${opts.label(tab)}: ${opts.errorText(err)}`,
        );
      }
    } finally {
      filling.delete(run.id);
    }
  }

  /** Takes a manual prompt's paste the person has not touched back out of the tab's Claude input. */
  async function clearPaste(tab: T) {
    if (!pastedTabs.delete(tab.tabId) || !(await opts.live(tab))) return;
    await opts.write(tab, encoder.encode("\x03"));
    dirtyTabs.delete(tab.tabId);
    backslashTabs.delete(tab.tabId);
    await new Promise((resolve) => setTimeout(resolve, CLEAR_INPUT_MS));
  }

  /** Why a prompt typed into `tab` was not sent, for the person. */
  const notSent = (tab: T) => `Claude left ${opts.label(tab)} before the prompt was sent, so it was not sent`;

  /**
   * Whether the person lets a flow a program in a tab added run: its steps go to
   * Claude unseen once it starts. Confirming it is stored, so ▶ asks only once.
   */
  async function confirmed(flow: Flow): Promise<boolean> {
    if (!flow.unconfirmed) return true;
    const sure = await ask(
      `"${flow.name}" was added from a tab with misag flow import, possibly by Claude, or its Start has text an older misag did not show. Its steps and Start's base info are sent to Claude as they are, so open it and read them before running it. Run it now?`,
      { title: "Run a flow added from a tab", kind: "warning", okLabel: "Run it", cancelLabel: "Not now" },
    );
    if (!sure) return false;
    try {
      await invoke("flow_confirm", { id: flow.id });
    } catch (err) {
      console.error("misag: could not confirm the flow", err);
      flows.showBoxError(`Could not confirm "${flow.name}": ${opts.errorText(err)}`);
      return false;
    }
    await flows.reload().catch(() => {}); // A failed read shows its own error.
    return true;
  }

  async function start(flow: Flow) {
    const tab = opts.active();
    // A start already under way here (it may be waiting for its clear) is the one.
    if (!tab || startingTabs.has(tab.tabId)) return;
    startingTabs.add(tab.tabId);
    try {
      // Locked from /clear to the briefing, so nothing typed lands in between.
      if (await confirmed(flow)) await withLock(tab, () => startIn(tab, flow));
    } finally {
      startingTabs.delete(tab.tabId);
    }
    await poll();
  }

  async function startIn(tab: T, flow: Flow) {
    if (!(await opts.live(tab))) {
      flows.showBoxError("Flows run in a Claude tab: select one (or a terminal tab running claude) and press ▶ again.");
      return;
    }
    if (opts.busy(tab)) {
      // The end of Claude's turn would be taken for its reply to the run's briefing
      // (and /clear would wait for it).
      flows.showBoxError(`Claude is working in ${opts.label(tab)}, so "${flow.name}" was not started; press ▶ again when it is idle.`);
      return;
    }
    if (!flow.clear && opts.waitsInBackground(tab)) {
      // The turn the tasks wake Claude for would be taken for its reply to the briefing.
      // A flow that clears first still starts: /clear ends the conversation they belong to.
      flows.showBoxError(
        `Claude in ${opts.label(tab)} is waiting on background tasks, so "${flow.name}" was not started; press ▶ again once they finish.`,
      );
      return;
    }
    let started: Started;
    try {
      // The run starts from the stored flow, so the editor's latest change goes first;
      // one that cannot be saved says so in the editor.
      for (const [flowId, editor] of opts.editors()) if (flowId === flow.id) await editor.flush();
      // The backend takes the tab before anything is typed, so a start it refuses
      // (the tab runs a flow, the flow is not ready) clears nothing.
      started = await invoke<Started>("flow_start", { flowId: flow.id, tabId: tab.tabId });
    } catch (err) {
      console.error("misag: could not start the flow", err);
      flows.showBoxError(`Could not start "${flow.name}": ${opts.errorText(err)}`);
      return;
    }
    flows.showBoxError(null);
    try {
      await sendStart(tab, flow, started);
    } catch (err) {
      console.error("misag: could not type into the run's tab", err);
      await stopStart(started.runId, `Could not type into ${opts.label(tab)} (${opts.errorText(err)}), so "${flow.name}" was stopped.`);
    }
  }

  /** Types a started run's /clear and briefing; throws when the tab does not take them. */
  async function sendStart(tab: T, flow: Flow, started: Started) {
    if (!started.clear) {
      if (started.prompt !== null && !(await typePrompt(tab, started.prompt))) await briefingUnsent(tab, flow, started.runId);
      return;
    }
    await poll();
    if (opts.busy(tab)) {
      // Claude would take /clear only after its turn, which may outlast the wait.
      await stopStart(started.runId, `Claude is working in ${opts.label(tab)}, so "${flow.name}" was not started; press ▶ again when it is idle.`);
      return;
    }
    if (!(await typePrompt(tab, "/clear"))) {
      await stopStart(started.runId, `Claude left ${opts.label(tab)} before /clear was sent, so "${flow.name}" was stopped.`);
      return;
    }
    for (let waited = 0; waited < CLEAR_TIMEOUT_MS; waited += CLEAR_POLL_MS) {
      await new Promise((resolve) => setTimeout(resolve, CLEAR_POLL_MS));
      let cleared: Cleared;
      try {
        cleared = await invoke<Cleared>("flow_cleared", { runId: started.runId });
      } catch (err) {
        console.error("misag: could not check the flow's clear", err);
        flows.showBoxError(`Could not go on with "${flow.name}" after /clear: ${opts.errorText(err)}. Stop the run and press ▶ again.`);
        return;
      }
      // Stopped meanwhile: nothing to do.
      if (cleared.state === "gone") return;
      if (cleared.state === "ready") {
        if (cleared.prompt !== null && !(await typePrompt(tab, cleared.prompt))) await briefingUnsent(tab, flow, started.runId);
        return;
      }
    }
    await stopStart(started.runId, `Claude did not clear the conversation in ${opts.label(tab)} within ${CLEAR_TIMEOUT_MS / 1000} seconds, so "${flow.name}" was stopped; press ▶ again when it is idle.`);
  }

  /**
   * Stops a run whose briefing could not be sent: the run waits for Claude's reply
   * to it, which will not come, and nothing else moves it on.
   */
  function briefingUnsent(tab: T, flow: Flow, runId: number) {
    return stopStart(runId, `${notSent(tab)}, so "${flow.name}" was stopped; start Claude there and press ▶ again.`);
  }

  /** Stops a run whose start could not go through its clear or send its briefing, saying why. */
  async function stopStart(runId: number, why: string) {
    try {
      await invoke("flow_stop", { runId });
      flows.showBoxError(why);
    } catch (err) {
      console.error("misag: could not stop the run", err);
      flows.showBoxError(`${why} It could not be stopped (${opts.errorText(err)}); stop it with ■.`);
    }
  }

  /**
   * Moves the run on with `go`, typing the prompt it returns into the run's tab;
   * resolves to why it could not, for the run bar it was picked in, or null. A
   * prompt that cannot be typed has the run wait at its step (see `typeStepPrompt`).
   */
  async function goOn(run: Run, go: () => Promise<StepPrompt | null>): Promise<string | null> {
    const tab = opts.tabs().find((t) => t.tabId === run.tabId);
    let error: string | null = null;
    if (!tab || !(await opts.live(tab))) {
      error = `Claude is not running in ${tab ? opts.label(tab) : "the run's tab"}; start it there first, or stop the run.`;
    } else if ((error = stillWorking(tab)) === null) {
      try {
        const prompt = await withLock(tab, async () => {
          // Claude's state comes from its terminal title, which may have changed while
          // this waited for the check above or for the lock.
          error = stillWorking(tab);
          if (error !== null) return null;
          const next = await go();
          // Gone on from a manual prompt: its unsent draft is no longer for anything.
          await clearPaste(tab);
          return next;
        });
        if (prompt !== null) error = await typeStepPrompt(run, tab, prompt);
      } catch (err) {
        console.error("misag: could not continue the run", err);
        error = `Could not go on: ${opts.errorText(err)}`;
      }
    }
    await poll();
    return error;
  }

  /**
   * Types the step's prompt `next` that the run went on to into its tab. One that
   * cannot be (Claude left the tab, or the write failed) has the run wait at the step
   * for the person, as no reply to it can come, and Continue sends it again. Resolves
   * to why that could not be done, for the run bar, or null: the run's note says why
   * it waits, in the run bar and the Flows box.
   */
  async function typeStepPrompt(run: Run, tab: T, next: StepPrompt): Promise<string | null> {
    let why = "Claude left the run's tab before this step's prompt was sent; start Claude there and press Continue to send it again.";
    try {
      if (await typePrompt(tab, next.prompt)) return null;
    } catch (err) {
      console.error("misag: could not type the step's prompt", err);
      why = `This step's prompt could not be typed into the run's tab (${opts.errorText(err)}); press Continue to send it again.`;
    }
    try {
      const waits = await invoke<boolean>("flow_unsent", { runId: run.id, stepId: next.stepId, briefs: next.briefs, why });
      return waits ? null : "The step's prompt was not sent, and the run had already moved on.";
    } catch (err) {
      console.error("misag: could not make the run wait for the person", err);
      return `The step's prompt was not sent, and the run could not be made to wait (${opts.errorText(err)}): pick an output to go on, or stop it with ■.`;
    }
  }

  /**
   * Why the run in `tab` cannot go on yet, or null: the end of the turn Claude is in,
   * or of the one its background tasks wake it for, would be taken for its reply to the
   * next step, and a prompt typed now would wait behind it. Esc reports no reply, so a
   * pick once Claude is idle goes on as it should.
   */
  function stillWorking(tab: T): string | null {
    if (opts.busy(tab)) {
      return `Claude is working in ${opts.label(tab)}; interrupt it (Esc) or wait for its reply, then go on, or stop the run with ■.`;
    }
    if (opts.waitsInBackground(tab)) {
      return `Claude in ${opts.label(tab)} is waiting on background tasks, which wake it for another turn; go on once they finish, or stop the run with ■.`;
    }
    return null;
  }

  async function stop(run: Run) {
    try {
      // Claude is told the flow is finished with the person's next prompt (the
      // UserPromptSubmit hook), so nothing is typed into a turn or a permission prompt.
      await invoke("flow_stop", { runId: run.id });
      flows.showBoxError(null);
      const tab = opts.tabs().find((t) => t.tabId === run.tabId);
      if (tab) await withLock(tab, () => clearPaste(tab));
    } catch (err) {
      console.error("misag: could not stop the run", err);
      flows.showBoxError(`Could not stop the run: ${opts.errorText(err)}`);
    }
    await poll();
  }

  /**
   * Shows the active tab's runs, and marks the tabs whose run waits for the person.
   * Replies can come back out of order: one older than a reply already shown is
   * dropped, as it would bring back runs that have moved on and redraw their rows.
   */
  async function poll() {
    const seq = ++polls;
    try {
      const next = await invoke<Run[]>("flow_runs");
      if (seq < shownPoll) return;
      shownPoll = seq;
      runs = next;
      if (runsError !== null) flows.showBoxError(null);
      runsError = null;
      show();
      waitingTabs.clear();
      for (const run of runs) {
        if (run.status !== "running") waitingTabs.add(run.tabId);
      }
      opts.markWaiting((tab) => waitingTabs.has(tab.tabId));
      for (const run of runs) if (run.prefill) void fill(run);
    } catch (err) {
      console.error("misag: could not read the flow runs", err);
      if (seq < shownPoll) return;
      shownPoll = seq;
      runsError = `Flow runs could not be read: ${opts.errorText(err)}`;
      flows.showBoxError(runsError);
    }
    await retitle();
  }

  /** Names the tabs Claude wrote titles for; each title is handed over once, and one for a tab that is gone is dropped. */
  async function retitle() {
    try {
      for (const { tabId, title } of await invoke<TabTitle[]>("flow_tab_titles")) {
        const tab = opts.tabs().find((t) => t.tabId === tabId);
        if (tab) opts.retitle(tab, title);
      }
    } catch (err) {
      // Left in the database: the next poll tries again.
      console.error("misag: could not read the tab titles", err);
    }
  }

  return {
    start,
    stop,
    // The step the run was shown at goes along, so a pick that arrives after the run
    // moved on (a second click, a page not yet redrawn) is refused, not applied there.
    proceed: (run, output) => goOn(run, () => invoke<StepPrompt | null>("flow_continue", { runId: run.id, stepId: run.stepId, output })),
    resume: (run) => goOn(run, () => invoke<StepPrompt>("flow_resume", { runId: run.id, stepId: run.stepId })),
    poll,
    show,
    forgetTab(tab) {
      runs = runs.filter((r) => r.tabId !== tab.tabId);
      backslashTabs.delete(tab.tabId);
    },
    waiting: (tab) => waitingTabs.has(tab.tabId),
    restored() {
      tabsRestored = true;
      show();
    },
    typed(tab, data) {
      // What the terminal reports by itself (focus, query replies, mouse) is not typing.
      const text = (typeof data === "string" ? data : decoder.decode(data)).replace(TERMINAL_REPORTS, "");
      // An Enter that is not inside a paste sends the input; Ctrl+C clears it. Alt+Enter
      // (ESC before it, in the same chunk) and an Enter after a backslash add a line instead.
      const before = text.length > 1 ? text.at(-2) : backslashTabs.has(tab.tabId) ? "\\" : undefined;
      const enter = text.endsWith("\r") && before !== "\x1b" && before !== "\\";
      const sent = (enter && !text.includes("\x1b[200~")) || text === "\x03";
      if (sent) dirtyTabs.delete(tab.tabId);
      else if (text) dirtyTabs.add(tab.tabId);
      if (text.endsWith("\\")) backslashTabs.add(tab.tabId);
      else if (text) backslashTabs.delete(tab.tabId);
      // A paste the person has added to, sent or cleared is theirs.
      if (text) pastedTabs.delete(tab.tabId);
    },
    locked: (tab) => lockCounts.has(tab.tabId),
  };
}
