import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import type { Flow, Flows, Run } from "./flows";

/** Mirrors `runs::Started` in src-tauri/src/runs.rs. */
export interface Started {
  runId: number;
  clear: boolean;
  prompt: string | null;
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
const CLEAR_TIMEOUT_MS = 30_000;

const encoder = new TextEncoder();

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
  tabs(): T[];
  /** The tab selected last. */
  active(): T | null;
  /** The tab's name, for messages. */
  label(tab: T): string;
  /** Whether Claude is working in the tab. */
  busy(tab: T): boolean;
  /**
   * Whether Claude runs in the tab right now, so a prompt typed there goes to Claude
   * and not to a shell.
   */
  live(tab: T): Promise<boolean>;
  /** Types into the tab's terminal. */
  write(tab: T, data: Uint8Array): void;
  /** The open flow editors, by flow id. */
  editors(): Iterable<[number, RunEditor]>;
  /** Marks in the tab bar the tabs whose run waits for the person. */
  markWaiting(isWaiting: (tab: T) => boolean): void;
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
  /** The error poll shows, so a later good poll clears only its own. */
  let runsError: string | null = null;

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
  async function typePrompt(tab: T, text: string): Promise<boolean> {
    const left = () => {
      console.warn(`misag: Claude left ${opts.label(tab)} before its flow prompt was sent; it was not sent`);
      return false;
    };
    if (!(await opts.live(tab))) return left();
    opts.write(tab, encoder.encode(`\x1b[200~${text}\x1b[201~`));
    await new Promise((resolve) => setTimeout(resolve, PASTE_SUBMIT_MS));
    if (!(await opts.live(tab))) return left();
    opts.write(tab, encoder.encode("\r"));
    return true;
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
      `"${flow.name}" was added from a tab with misag flow import, possibly by Claude. Its steps are sent to Claude as they are, so open it and read them before running it. Run it now?`,
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
      if (await confirmed(flow)) await startIn(tab, flow);
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
    if (!started.clear) {
      if (started.prompt !== null && !(await typePrompt(tab, started.prompt))) {
        flows.showBoxError(`${notSent(tab)}. Start Claude there and pick an output to go on, or stop the run.`);
      }
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
        if (cleared.prompt !== null && !(await typePrompt(tab, cleared.prompt))) {
          flows.showBoxError(`${notSent(tab)}. Start Claude there and pick an output to go on, or stop the run.`);
        }
        return;
      }
    }
    await stopStart(started.runId, `Claude did not clear the conversation in ${opts.label(tab)} within ${CLEAR_TIMEOUT_MS / 1000} seconds, so "${flow.name}" was stopped; press ▶ again when it is idle.`);
  }

  /** Stops a run whose start could not go through its clear, saying why. */
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
   * resolves to why it could not, for the run bar it was picked in, or null.
   */
  async function goOn(run: Run, go: () => Promise<string | null>): Promise<string | null> {
    const tab = opts.tabs().find((t) => t.tabId === run.tabId);
    let error: string | null = null;
    if (!tab || !(await opts.live(tab))) {
      error = `Claude is not running in ${tab ? opts.label(tab) : "the run's tab"}; start it there first, or stop the run.`;
    } else {
      try {
        const prompt = await go();
        if (prompt !== null && !(await typePrompt(tab, prompt))) error = `${notSent(tab)}; start Claude there and go on again.`;
      } catch (err) {
        console.error("misag: could not continue the run", err);
        error = `Could not go on: ${opts.errorText(err)}`;
      }
    }
    await poll();
    return error;
  }

  async function stop(run: Run) {
    try {
      await invoke("flow_stop", { runId: run.id });
      flows.showBoxError(null);
    } catch (err) {
      console.error("misag: could not stop the run", err);
      flows.showBoxError(`Could not stop the run: ${opts.errorText(err)}`);
    }
    await poll();
  }

  /** Shows the active tab's runs, and marks the tabs whose run waits for the person. */
  async function poll() {
    try {
      runs = await invoke<Run[]>("flow_runs");
      if (runsError !== null) flows.showBoxError(null);
      runsError = null;
      show();
      waitingTabs.clear();
      for (const run of runs) {
        if (run.status !== "running") waitingTabs.add(run.tabId);
      }
      opts.markWaiting((tab) => waitingTabs.has(tab.tabId));
    } catch (err) {
      console.error("misag: could not read the flow runs", err);
      runsError = `Flow runs could not be read: ${opts.errorText(err)}`;
      flows.showBoxError(runsError);
    }
  }

  return {
    start,
    stop,
    // The step the run was shown at goes along, so a pick that arrives after the run
    // moved on (a second click, a page not yet redrawn) is refused, not applied there.
    proceed: (run, output) => goOn(run, () => invoke<string | null>("flow_continue", { runId: run.id, stepId: run.stepId, output })),
    resume: (run) => goOn(run, () => invoke<string>("flow_resume", { runId: run.id, stepId: run.stepId })),
    poll,
    show,
    forgetTab(tab) {
      runs = runs.filter((r) => r.tabId !== tab.tabId);
    },
    waiting: (tab) => waitingTabs.has(tab.tabId),
    restored() {
      tabsRestored = true;
      show();
    },
  };
}
