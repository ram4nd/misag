import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import type { Channel } from "@tauri-apps/api/core";
import type { WTerm } from "@wterm/dom";
import indexHtml from "../index.html?raw";
import capabilities from "../src-tauri/capabilities/default.json?raw";
import tauriConfig from "../src-tauri/tauri.conf.json?raw";
import sharedLayout from "../src-tauri/tests/fixtures/layout.json?raw";

// wterm needs WASM and real layout, so the tests drive a stand-in that
// records writes and exposes the callbacks tabs.ts and terminalView.ts assign.
const fakes = vi.hoisted(() => {
  type Used = "cols" | "rows" | "element" | "onData" | "onBinary" | "onTitle" | "onResize" | "write" | "fit" | "focus" | "destroy" | "setRenderingPaused" | "bridge" | "getSelectionText";
  class FakeTerminal implements Pick<WTerm, Used> {
    static all: FakeTerminal[] = [];
    /** Makes the next terminal's init() fail, as a broken view would. */
    static failOnce = false;
    /** Keeps each init() waiting until the test settles it through `loading`. */
    static hold = false;
    cols = 80;
    rows = 24;
    /** Whether the program in it asked for bracketed paste. */
    bracketed = false;
    /** The mouse tracking mode the program in it asked for (0: none). */
    tracking = 0;
    bridge = { bracketedPaste: () => this.bracketed, mouseTracking: () => this.tracking } as unknown as WTerm["bridge"];
    written: string[] = [];
    /** The text wterm holds selected in it, apart from the page's own selection. */
    selected: string | null = null;
    getSelectionText() {
      return this.selected;
    }
    destroyed = false;
    paused: boolean;
    /** Focus calls that reached the terminal's input, which exists only once it has loaded. */
    focused = 0;
    loaded = false;
    loading: { resolve(): void; reject(err: Error): void } | null = null;
    options: Record<string, unknown>;
    onData: ((data: string) => void) | null = null;
    onBinary: ((data: Uint8Array) => void) | null = null;
    onTitle: ((title: string) => void) | null = null;
    onResize: ((cols: number, rows: number) => void) | null = null;
    constructor(public element: HTMLElement, options: Record<string, unknown> = {}) {
      this.options = { ...options };
      this.paused = Boolean(options.renderingPaused);
      // Like wterm: a plain click on a link stays terminal interaction; Ctrl+click or the keyboard opens it.
      element.addEventListener("click", (e) => {
        const onLink = e.target instanceof Element && e.target.closest(".term-link");
        if (onLink && !e.ctrlKey && !e.metaKey && e.detail !== 0) e.preventDefault();
      });
      FakeTerminal.all.push(this);
    }
    get output() {
      return this.written.join("");
    }
    async init() {
      try {
        if (FakeTerminal.hold) await new Promise<void>((resolve, reject) => (this.loading = { resolve, reject }));
        if (FakeTerminal.failOnce) {
          FakeTerminal.failOnce = false;
          throw new Error("no wasm");
        }
      } catch (err) {
        // Like wterm: a failed load cleans up after itself.
        this.destroy();
        throw new Error(`wterm: failed to initialize: ${(err as Error).message}`);
      }
      // Like wterm: a terminal destroyed while loading resolves without an error.
      if (!this.destroyed) this.loaded = true;
      // Like wterm without autoResize: the pane is held at the height of its first rows.
      if (!this.destroyed && this.options.autoResize === false) this.element.style.height = "420px";
      return this;
    }
    write(data: string | Uint8Array) {
      // A full reset (RIS) clears the screen.
      if (data === "\x1bc") this.written = [];
      else this.written.push(typeof data === "string" ? data : new TextDecoder().decode(data));
    }
    /** fit() calls, which measure the font again. */
    fits = 0;
    fit() {
      this.fits++;
    }
    focus() {
      if (this.loaded) this.focused++;
    }
    setRenderingPaused(paused: boolean) {
      this.paused = paused;
    }
    destroy() {
      this.destroyed = true;
    }
  }
  return { FakeTerminal };
});

// The window is faked directly: the IPC mocks cannot deliver window events.
const win = vi.hoisted(() => ({
  closeRequested: undefined as undefined | (() => Promise<unknown>),
  /** Whether the last close request was cancelled. */
  prevented: false,
  /** How often the window was brought to the front. */
  focused: 0,
  /** How often the close listener was removed. */
  unlistened: 0,
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: async (handler: (event: { preventDefault(): void }) => Promise<unknown>) => {
      win.closeRequested = () => {
        win.prevented = false;
        return handler({ preventDefault: () => (win.prevented = true) });
      };
      return () => void win.unlistened++;
    },
    setFocus: async () => {
      win.focused++;
    },
  }),
}));

// The webview is faked like the window above: `drop.emit` sends it a drag-and-drop event
// as a file manager would, and `drop.fails` makes listening for them fail.
const drop = vi.hoisted(() => ({
  emit: undefined as undefined | ((payload: { type: string; paths?: string[] }) => void),
  fails: false,
  /** How often the drop listener was removed. */
  unlistened: 0,
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: async (handler: (event: { payload: { type: string; paths?: string[] } }) => void) => {
      if (drop.fails) throw new Error("no events");
      drop.emit = (payload) => handler({ payload });
      return () => void drop.unlistened++;
    },
  }),
}));

// The folder picker answers with `picker.picked` (null: cancelled), or fails if `picker.fails`.
const picker = vi.hoisted(() => ({
  picked: null as string | null,
  fails: false,
  calls: [] as { defaultPath?: string }[],
  /** The answer to a yes/no question, and the questions asked. */
  answer: false,
  asked: [] as string[],
  /** Makes asking fail, as a dialog that cannot open would. */
  askFails: false,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: async (options: { defaultPath?: string }) => {
    picker.calls.push(options);
    if (picker.fails) throw new Error("no portal");
    return picker.picked;
  },
  ask: async (message: string) => {
    picker.asked.push(message);
    if (picker.askFails) throw new Error("no portal");
    return picker.answer;
  },
}));

vi.mock("@wterm/dom/src/terminal.css", () => ({}));
vi.mock("@wterm/dom", () => ({ WTerm: fakes.FakeTerminal }));
/** The sounds played, by name, and how often audio was primed: jsdom has no Web Audio. `fail` makes playing fail. */
const sounds = vi.hoisted(() => ({ played: [] as string[], primed: 0, fail: null as Error | null }));
vi.mock("./sounds", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./sounds")>()),
  playSound: (name: string, onFail: (err: unknown) => void) => {
    if (sounds.fail) onFail(sounds.fail);
    else sounds.played.push(name);
  },
  primeAudio: () => void sounds.primed++,
}));

type Call = { cmd: string; args: Record<string, unknown> };
type SpawnCall = {
  fallbackToHome: boolean;
  kind: "terminal" | "claude";
  tabId: string;
  cwd: string | null;
  command: string | null;
  claudeSession: string | null;
  onExit: Channel<number>;
};
type Layout = {
  tabs: {
    tabId: string;
    kind: "terminal" | "claude";
    name: string;
    renamed: boolean;
    cwd: string | null;
    command: string | null;
    claudeSession: string | null;
  }[];
  active: number;
};

/** How often the app checks tab directories; mirrors TAB_POLL_MS in tabPoller.ts. */
const TAB_POLL_MS = 2_000;
/** How often the app asks whether Claude has cleared, and for how long; mirrors CLEAR_POLL_MS and CLEAR_TIMEOUT_MS in runController.ts. */
const CLEAR_POLL_MS = 250;
const CLEAR_TIMEOUT_MS = 30_000;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const SESSION = "0f8fad5b-d9cb-469f-a165-70867728950e";
const LOG = "/home/me/.local/share/io.github.ram4nd.misag/hook.log";

let calls: Call[];
let spawnBehaviour: (args: SpawnCall, id: number) => number | Promise<number>;
let savedLayout: Layout;
let liveCwds: Record<number, string>;
/** The Claude session claude_open_sessions finds open, by PTY id; other tabs are closed unless in `claudeStates`. */
let openClaudes: Record<number, string>;
/** Other answers of claude_open_sessions, by PTY id. */
let claudeStates: Record<number, { state: "unknown" } | { state: "untracked"; pid: number; skipsPermissions?: boolean }>;
/** PTY ids whose open Claude was started with --dangerously-skip-permissions. */
let skippingClaudes: Set<number>;
/** What background_tabs reports: when each tab's Claude last ended a turn waiting on background tasks (ms), by tab id. */
let backgroundTabs: Record<string, number>;
/** claude_open_sessions waits for this, so a test can hold a check back. */
let claudeCheckGate: Promise<void>;
/** What pty_spawn reports about a terminal tab's saved Claude session. */
let resumeReport: { state: "typed" } | { state: "noConversation" } | { state: "failed"; message: string };
let failing: Set<string>;
let storeReport: {
  store: { state: "saving" } | { state: "recovered"; notice: string } | { state: "notSaving"; reason: string };
  hookProblem: { at: number; message: string; log: string } | null;
  wrapperProblem?: string | null;
  cliProblem?: string | null;
};
let reported: Set<string>;
let missingDirs: Set<string>;
type Settings = Record<string, unknown>;
let storedSettings: Settings;
/** The callbacks of every ResizeObserver the app made, to report a resize to. */
let resizeObservers: ResizeObserverCallback[];
/** How many ResizeObservers the app disconnected. */
let resizeDisconnects: number;
/** fs_list for a directory waits for its promise here, so a test can hold it back. */
let fsGates: Record<string, Promise<void>>;
/** How many hidden entries fs_list reports per directory. */
let fsHidden: Record<string, number>;
/** What fs_list returns per directory; any other directory fails. */
let fsTree: Record<string, { name: string; dir: boolean }[]>;
/** settings_load waits for this, so a test can hold the load back. */
let settingsLoadGate: Promise<void>;
const DEFAULTS: Settings = { skipPermissions: false, workspace: null, disableAutoMemory: false, fontSize: 14, finishSound: "off" };
type Flow = { id: number; name: string; archived: boolean; unconfirmed?: boolean };
/** The stored flows in order, archived ones included; the flow commands change it like the backend. */
let storedFlows: Flow[];
/** Flows that flow_delete refuses, as the runner will for running ones. */
let busyFlows: Set<number>;
type Step = { id: string; kind: string; title: string; text: string; outputs: string[]; clear?: boolean; x: number; y: number };
type Graph = { nodes: Step[]; edges: { from: string; output: string; to: string }[] };
const START: Step = { id: "start", kind: "start", title: "Start", text: "", outputs: ["next"], x: 40, y: 40 };
/** Each flow's stored graph; a flow without one has only its Start step. */
let storedGraphs: Record<number, Graph>;
/** What flow_load and flow_save_graph report as keeping the flow from running. */
let flowProblems: string[];
type Run = {
  id: number; flowId: number; flowName: string; tabId: string; stepId: string; step: string;
  status: "running" | "manual" | "attention"; note: string | null; outputs: string[]; steps: number; done: number; total: number; resumable: boolean; prefill: boolean;
  stepKind: "start" | "prompt" | "manual" | "manual_prompt" | "end" | null; briefing: boolean;
};
/** What flow_runs reports. */
let activeRuns: Run[];
/** The prompt flow_start and flow_continue return; a string makes them fail with it. */
let runPrompt: string | null;
let runRefusal: string | null;
/** Makes flow_prefill_text fail with it. */
let prefillError: string | null;
/** Makes flow_prefilled fail with it. */
let prefilledError: string | null;
/** What flow_prefill_text returns, one per call; then null. */
let prefills: ({ text: string; token: number } | null)[];
/** What flow_cleared reports, one per call; then "pending". */
let clearStates: ({ state: "pending" | "gone" } | { state: "ready"; prompt: string | null })[];
/** What primary_selection reports: the text selected anywhere on the desktop, if any. */
let primarySelection: string | null;
/** What clipboard_text reports: the text copied to the clipboard, if any. */
let clipboard: string | null;
/** Makes flow_import_file and flow_export_file fail with it. */
let importRefusal: string | null;
/** Whether the person picks a file in the import or export dialog (which the backend opens). */
let flowPicked: boolean;
type Past = { id: string; title: string; cwd: string; modified: number; pinned?: boolean };
/** What claude_history lists: the sessions of every folder, newest first. */
let pastSessions: Past[];
/** How many transcripts claude_history reports it could not read. */
let historyUnreadable: number;
/** The sessions pinned with claude_session_pin, listed first by claude_history. */
let pinnedSessions: Set<string>;
/** Gates the next claude_history calls wait on, one each, in order. */
let historyGates: Promise<void>[];
/** The session each tab's hook reported, by tab id (claude_tab_sessions). */
let tabSessions: Record<string, string>;

const terms = () => fakes.FakeTerminal.all;
const tabs = () => [...document.querySelectorAll<HTMLElement>("#tabs .tab")];
const tabNames = () => tabs().map((t) => t.querySelector(".name")?.textContent);
const callsOf = (cmd: string) => calls.filter((c) => c.cmd === cmd);
// pty_spawn takes `{ request, onData, onExit }`; flatten it for assertions.
const spawnCall = (args: Record<string, unknown>) =>
  ({ ...(args.request as object), onExit: args.onExit }) as SpawnCall;
const spawns = () => callsOf("pty_spawn").map((c) => spawnCall(c.args));
const dialog = () => document.querySelector<HTMLDialogElement>("#spawn-dialog")!;
const field = (name: string) =>
  dialog().querySelector("form")!.elements.namedItem(name) as HTMLInputElement | null;
const noTabs = () => document.body.classList.contains("no-tabs");
const notice = () => document.querySelector<HTMLDivElement>("#notice")!;
const noticeVisible = () => !notice().hidden;
const noticeText = () => notice().querySelector("span")!.textContent ?? "";
const dismissButton = () => notice().querySelector<HTMLButtonElement>("button")!;
const flush = () =>
  vi.isFakeTimers() ? vi.advanceTimersByTimeAsync(0) : new Promise((r) => setTimeout(r, 0));
const lastSaved = () => callsOf("layout_save").at(-1)?.args.layout as Layout | undefined;

/**
 * Stops the app the last `startApp` started, as main.boot()'s dispose does: it clears its
 * timers, removes its window, document and Tauri listeners, disconnects its ResizeObserver,
 * and keeps its pending callbacks from changing anything.
 */
let dispose: (() => void) | null = null;

/** Loads index.html into the page and starts the app on it with main.boot(), then lets startup settle. */
async function startApp() {
  dispose?.();
  document.body.innerHTML = new DOMParser().parseFromString(indexHtml, "text/html").body.innerHTML;
  const main = await import("./main");
  dispose = main.boot();
  for (let i = 0; i < 5; i++) await flush();
}

/** Clicks the new-terminal button at the top right; Shift opens the dialog. */
async function clickTerminal(shiftKey = false) {
  const button = document.querySelector<HTMLButtonElement>("#new-terminal")!;
  button.dispatchEvent(new MouseEvent("click", { bubbles: true, shiftKey }));
  for (let i = 0; i < 3; i++) await flush();
}

/** Opens a Claude tab from the Claude button of the workspace's first folder (by default ~/work). */
async function clickClaude() {
  const button = document.querySelector<HTMLButtonElement>("#workspace-box .tree > ul .open-claude");
  if (!button) throw new Error("no folder with a Claude button in the workspace tree");
  button.click();
  for (let i = 0; i < 3; i++) await flush();
}

async function openClaudeTabs(n: number) {
  for (let i = 0; i < n; i++) await clickClaude();
}

function key(el: HTMLElement, key: string) {
  el.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
}

function renameTab(i: number, name: string, finish = "Enter") {
  tabs()[i].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  const input = tabs()[i].querySelector("input")!;
  input.value = name;
  key(input, finish);
  input.dispatchEvent(new FocusEvent("blur"));
}

/** The flow commands, like the backend: names are trimmed, new flows go last. */
function flowCommand(cmd: string, args: Record<string, unknown>) {
  const find = (id: unknown) => {
    const flow = storedFlows.find((f) => f.id === id);
    if (!flow) throw { code: "failed", message: `there is no flow ${id}` };
    return flow;
  };
  const named = (name: unknown) => {
    const trimmed = String(name).trim();
    if (!trimmed) throw { code: "failed", message: "a flow needs a name" };
    return trimmed;
  };
  switch (cmd) {
    case "flows_list":
      // As the backend reads it from each flow's Start step.
      return storedFlows.map((f) => (storedGraphs[f.id]?.nodes.some((n) => n.kind === "start" && n.clear) ? { ...f, clear: true } : { ...f }));
    case "flow_create": {
      const flow = { id: Math.max(0, ...storedFlows.map((f) => f.id)) + 1, name: named(args.name), archived: false };
      storedFlows.push(flow);
      return { ...flow };
    }
    case "flow_rename":
      find(args.id).name = named(args.name);
      return null;
    case "flow_set_archived":
      find(args.id).archived = args.archived as boolean;
      return null;
    case "flows_reorder": {
      const ids = args.ids as number[];
      const same = [...ids].sort().join() === storedFlows.map((f) => f.id).sort().join();
      if (!same) throw { code: "failed", message: "the order must list every flow once" };
      storedFlows = ids.map(find);
      return null;
    }
    case "flow_load": {
      const flow = find(args.id);
      return { ...flow, graph: structuredClone(storedGraphs[flow.id] ?? { nodes: [START], edges: [] }), problems: flowProblems };
    }
    case "flow_save_graph":
      storedGraphs[find(args.id).id] = structuredClone(args.graph as Graph);
      return flowProblems;
    case "flow_start": {
      if (runRefusal) throw { code: "failed", message: runRefusal };
      // As the backend reads it from the flow's stored Start step.
      const clear = storedGraphs[args.flowId as number]?.nodes.some((n) => n.kind === "start" && n.clear) ?? false;
      return { runId: 5, clear, prompt: clear ? null : runPrompt };
    }
    case "flow_cleared":
      return clearStates.shift() ?? { state: "pending" };
    case "flow_prefill_text":
      if (prefillError) throw { code: "failed", message: prefillError };
      return prefills.shift() ?? null;
    case "flow_prefilled":
      if (prefilledError) throw { code: "failed", message: prefilledError };
      return null;
    case "flow_continue":
    case "flow_resume":
      if (runRefusal) throw { code: "failed", message: runRefusal };
      return runPrompt === null ? null : { stepId: "p", prompt: runPrompt, briefs: false };
    case "flow_runs":
      return activeRuns;
    case "flow_import_file": {
      if (importRefusal) throw { code: "failed", message: importRefusal };
      if (!flowPicked) return null;
      const flow = { id: Math.max(0, ...storedFlows.map((f) => f.id)) + 1, name: "Imported", archived: false };
      storedFlows.push(flow);
      return flow;
    }
    case "flow_export_file":
      find(args.id);
      if (importRefusal) throw { code: "failed", message: importRefusal };
      return flowPicked;
    case "flow_confirm":
      find(args.id).unconfirmed = undefined;
      return null;
    case "flow_stop":
      activeRuns = activeRuns.filter((r) => r.id !== args.runId);
      return null;
    case "flow_stop_tab":
      activeRuns = activeRuns.filter((r) => r.tabId !== args.tabId);
      return null;
    case "flow_delete":
      find(args.id);
      if (busyFlows.has(args.id as number)) throw { code: "failed", message: "the flow is running in a tab" };
      storedFlows = storedFlows.filter((f) => f.id !== args.id);
      return null;
  }
  throw new Error(`unmocked ${cmd}`);
}

beforeEach(() => {
  // Fake timers for every test: they are discarded afterwards, so a debounced save
  // left over from one test's app instance cannot fire during the next test.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
  vi.resetModules();
  fakes.FakeTerminal.all = [];
  fakes.FakeTerminal.failOnce = false;
  fakes.FakeTerminal.hold = false;
  calls = [];
  let nextId = 0;
  spawnBehaviour = (_args, id) => id;
  savedLayout = { tabs: [], active: 0 };
  liveCwds = {};
  openClaudes = {};
  claudeStates = {};
  skippingClaudes = new Set();
  backgroundTabs = {};
  claudeCheckGate = Promise.resolve();
  resumeReport = { state: "typed" };
  failing = new Set();
  drop.fails = false;
  drop.unlistened = 0;
  win.focused = 0;
  win.unlistened = 0;
  storeReport = { store: { state: "saving" }, hookProblem: null };
  reported = new Set();
  missingDirs = new Set();
  storedSettings = { ...DEFAULTS };
  sounds.played = [];
  sounds.primed = 0;
  sounds.fail = null;
  settingsLoadGate = Promise.resolve();
  storedFlows = [];
  busyFlows = new Set();
  storedGraphs = {};
  flowProblems = [];
  activeRuns = [];
  runPrompt = null;
  runRefusal = null;
  prefills = [];
  prefillError = null;
  prefilledError = null;
  clearStates = [];
  pastSessions = [];
  historyUnreadable = 0;
  pinnedSessions = new Set();
  historyGates = [];
  tabSessions = {};
  fsTree = { "~": [{ name: "work", dir: true }] };
  picker.picked = null;
  picker.fails = false;
  picker.calls = [];
  picker.answer = false;
  picker.askFails = false;
  importRefusal = null;
  flowPicked = true;
  primarySelection = null;
  clipboard = null;
  picker.asked = [];
  fsGates = {};
  fsHidden = {};
  win.closeRequested = undefined;
  win.prevented = false;
  try {
    localStorage.clear();
  } catch {
    // Storage may be unavailable here; tests that need it would then fail.
  }
  mockIPC((cmd, args) => {
    calls.push({ cmd, args: (args ?? {}) as Record<string, unknown> });
    if (failing.has(cmd)) throw `${cmd} failed`;
    if (cmd === "pty_spawn") {
      // Like the backend: a missing directory fails, or falls back to $HOME if asked to.
      const call = spawnCall(args as Record<string, unknown>);
      const missing = call.cwd !== null && missingDirs.has(call.cwd);
      if (missing && !call.fallbackToHome) {
        throw { code: "cwd_missing", message: `directory does not exist: ${call.cwd}` };
      }
      const id = spawnBehaviour(missing ? { ...call, cwd: null } : call, ++nextId);
      // Like the backend: only Claude tabs with a database are tracked.
      const tracked = call.kind === "claude" && storeReport.store.state !== "notSaving";
      const skipsPermissions = call.kind === "claude" && storedSettings.skipPermissions === true;
      const resume = call.kind === "terminal" && call.claudeSession ? resumeReport : null;
      return Promise.resolve(id).then((id) => ({ id, fellBackToHome: missing, tracked, skipsPermissions, startedAt: 1000, resume }));
    }
    if (cmd === "plugin:path|resolve_directory") return "/home/me";
    if (cmd === "store_status") return storeReport;
    if (cmd === "claude_reported") return reported.has((args as { tabId: string }).tabId);
    if (cmd === "layout_load") return savedLayout;
    if (cmd === "fs_list") {
      const dir = (args as { dir: string }).dir;
      return (fsGates[dir] ?? Promise.resolve()).then(() => {
        if (!(dir in fsTree)) throw { code: "failed", message: `cannot read ${dir}: No such file or directory` };
        const listed = dir === "~" ? "/home/me" : dir;
        return { dir: listed, entries: fsTree[dir], truncated: false, skipped: fsHidden[dir] ?? 0 };
      });
    }
    if (cmd === "claude_history") {
      const { limit } = args as { limit: number };
      const gate = historyGates.shift() ?? Promise.resolve();
      // Read when called, answered when the gate opens.
      const page = pastSessions.slice();
      const pins = new Set(pinnedSessions);
      return gate.then(() => {
        const pinned = page.filter((p) => pins.has(p.id)).map((p) => ({ ...p, pinned: true }));
        const rest = page.filter((p) => !pins.has(p.id)).map((p) => ({ ...p, pinned: false }));
        return { sessions: [...pinned, ...rest.slice(0, limit)], more: rest.length > limit, unreadable: historyUnreadable };
      });
    }
    if (cmd === "claude_session_pin") {
      const { id, pinned } = args as { id: string; pinned: boolean };
      if (pinned) pinnedSessions.add(id);
      else pinnedSessions.delete(id);
      return null;
    }
    if (cmd === "claude_tab_sessions") return tabSessions;
    if (cmd === "settings_load") return settingsLoadGate.then(() => storedSettings);
    if (cmd === "settings_save") {
      // Like the backend (without its trimming): a changed workspace folder must exist.
      const next = (args as { settings: Settings }).settings;
      const changed = next.workspace !== storedSettings.workspace;
      if (changed && typeof next.workspace === "string" && !(next.workspace in fsTree)) {
        throw { code: "failed", message: `${next.workspace} is not a folder` };
      }
      storedSettings = next;
      return storedSettings;
    }
    if (cmd.startsWith("flow")) return flowCommand(cmd, (args ?? {}) as Record<string, unknown>);
    if (cmd === "primary_selection") return primarySelection;
    if (cmd === "clipboard_text") return clipboard;
    if (cmd === "claude_open_sessions") {
      const ids = (args as { tabs: { id: number }[] }).tabs.map((t) => t.id);
      return claudeCheckGate.then(() => Object.fromEntries(
        ids.map((id) => [
          id,
          id in openClaudes
            ? { state: "open", session: openClaudes[id], skipsPermissions: skippingClaudes.has(id) }
            : (claudeStates[id] ?? { state: "closed" }),
        ]),
      ));
    }
    if (cmd === "background_tabs") {
      if (failing.has(cmd)) throw "background_tabs failed";
      return backgroundTabs;
    }
    if (cmd === "pty_cwds") {
      // Like the backend, only the requested sessions are answered.
      const ids = (args as { ids: number[] }).ids;
      return Object.fromEntries(Object.entries(liveCwds).filter(([id]) => ids.includes(Number(id))));
    }
  });

  resizeObservers = [];
  resizeDisconnects = 0;
  globalThis.ResizeObserver = class {
    constructor(cb: ResizeObserverCallback) {
      resizeObservers.push(cb);
    }
    observe() {}
    unobserve() {}
    disconnect() {
      resizeDisconnects++;
    }
  };
  globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0) as unknown as number;
  // jsdom has no modal dialog support.
  HTMLDialogElement.prototype.showModal = function () {
    this.open = true;
  };
  HTMLDialogElement.prototype.close = function (value?: string) {
    if (value !== undefined) this.returnValue = value;
    this.open = false;
    this.dispatchEvent(new Event("close"));
  };
});

afterEach(() => {
  dispose?.();
  dispose = null;
  clearMocks();
  // Spies (such as the layout getters a test stubs) do not restore themselves.
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("tabs", () => {
  test("starts with no tabs and an empty-state hint", async () => {
    await startApp();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
    expect(callsOf("pty_spawn")).toHaveLength(0);
  });

  test("the terminal button sits after the tabs and opens a terminal tab", async () => {
    await startApp();
    const bar = document.querySelector("#tabbar")!;
    expect(bar.lastElementChild?.id).toBe("tab-actions");
    expect(document.querySelector("#new-terminal svg")).not.toBeNull();
    await clickTerminal();
    expect(tabNames()).toEqual(["shell 1"]);
    expect(tabs()[0].querySelector<HTMLElement>(".icon")!.dataset.kind).toBe("terminal");
    expect(noTabs()).toBe(false);
    expect(spawns()[0]).toMatchObject({ kind: "terminal", command: null, claudeSession: null, cwd: null });
    expect(spawns()[0].tabId).toMatch(UUID);
  });

  test("a folder's Claude button opens a Claude tab in that folder", async () => {
    await startApp();
    const button = document.querySelector<HTMLButtonElement>("#workspace-box .tree > ul .open-claude")!;
    expect(button.title).toBe("Open Claude in /home/me/work");
    expect(button.closest(".line")!.querySelector("button.row")!.textContent).toBe("work");
    await clickClaude();
    expect(tabNames()).toEqual(["work"]);
    expect(tabs()[0].querySelector(".icon svg")).not.toBeNull();
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "/home/me/work", fallbackToHome: false });
    expect(spawns()[0]).not.toHaveProperty("command");
    expect(spawns()[0].claudeSession).toMatch(UUID);
    // The folder itself stays closed.
    expect(callsOf("fs_list").map((c) => c.args.dir)).toEqual(["~"]);
  });

  test("the workspace folder itself has a Claude button too", async () => {
    await startApp();
    const root = document.querySelector<HTMLButtonElement>("#workspace-box .tree .row.root + .open-claude")!;
    expect(root.title).toBe("Open Claude in ~");
    root.click();
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "~" });
  });

  test("each Claude tab gets its own session", async () => {
    await startApp();
    await openClaudeTabs(2);
    const [a, b] = spawns().map((s) => s.claudeSession);
    expect(a).not.toBe(b);
  });

  test("a new terminal tab opens in the active tab's current directory", async () => {
    await startApp();
    await clickClaude();
    liveCwds = { 1: "/home/me/src" };
    await clickTerminal();
    expect(spawns().map((s) => s.cwd)).toEqual(["/home/me/work", "/home/me/src"]);
  });

  test("closing the same tab twice kills it once and keeps the others", async () => {
    await startApp();
    await openClaudeTabs(3);
    const x = tabs()[1].querySelector<HTMLButtonElement>(".x")!;
    x.click();
    x.click();
    expect(callsOf("pty_kill").map((c) => c.args.id)).toEqual([2]);
    expect(tabNames()).toEqual(["work", "work"]);
    expect(terms()[1].destroyed).toBe(true);
  });

  test("middle-click closes a tab", async () => {
    await startApp();
    await openClaudeTabs(2);
    tabs()[0].dispatchEvent(new MouseEvent("auxclick", { button: 1, bubbles: true }));
    expect(callsOf("pty_kill").map((c) => c.args.id)).toEqual([1]);
    expect(tabs()).toHaveLength(1);
  });

  test("closing the last tab keeps the window open and shows the hint", async () => {
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
  });

  test("titles show in the tooltip until the user renames the tab", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("✳ Fix the build");
    expect(tabs()[0].title).toContain("✳ Fix the build");
    renameTab(0, "build");
    expect(tabNames()).toEqual(["build"]);
    terms()[0].onTitle!("✳ Something else");
    expect(tabNames()).toEqual(["build"]);
    expect(tabs()[0].title).not.toContain("Something else");
  });

  test("Escape cancels a rename", async () => {
    await startApp();
    await clickClaude();
    renameTab(0, "discarded", "Escape");
    expect(tabNames()).toEqual(["work"]);
  });
});

describe("tab labels", () => {
  const tooltip = (i: number) => tabs()[i].title;

  test("a tab is labelled with its folder's name", async () => {
    await startApp();
    await clickClaude();
    expect(tabNames()).toEqual(["work"]);
  });

  test("the label follows the tab's current directory", async () => {
    await startApp();
    await clickClaude();
    liveCwds[1] = "/home/me/src/misag";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabNames()).toEqual(["misag"]);
    liveCwds[1] = "/home/me";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabNames()).toEqual(["~"]);
  });

  test("the terminal title and the full directory are in the tooltip", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("✳ Fix the build");
    expect(tabNames()).toEqual(["work"]);
    expect(tooltip(0)).toContain("✳ Fix the build");
    expect(tooltip(0)).toContain("/home/me/work");
  });

  test("the rename box starts with what the tab shows", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("✳ Fix the build");
    tabs()[0].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    expect(tabs()[0].querySelector("input")!.value).toBe("work");
  });

  test("a rename left unchanged keeps the tab following its folder", async () => {
    await startApp();
    await clickClaude();
    renameTab(0, "work");
    liveCwds[1] = "/home/me/src";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabNames()).toEqual(["src"]);
  });

  test("a check while renaming keeps the rename box open", async () => {
    await startApp();
    await clickTerminal();
    tabs()[0].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    openClaudes[1] = SESSION;
    liveCwds[1] = "/home/me/src";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    const input = tabs()[0].querySelector("input")!;
    expect(input).not.toBeNull();
    key(input, "Escape");
    expect(tabs()[0].querySelector<HTMLElement>(".icon")!.dataset.kind).toBe("claude");
    expect(tabNames()).toEqual(["src"]);
  });

  test("a renamed tab keeps its name wherever it goes", async () => {
    await startApp();
    await clickClaude();
    renameTab(0, "build");
    liveCwds[1] = "/home/me/src";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabNames()).toEqual(["build"]);
  });

  test("a tab whose directory is not known yet shows its name", async () => {
    await startApp();
    await clickTerminal();
    expect(tabNames()).toEqual(["shell 1"]);
    liveCwds[1] = "/home/me/app";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabNames()).toEqual(["app"]);
  });

  test("exited tabs are not polled", async () => {
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(0);
    const before = callsOf("pty_cwds").length;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS * 3);
    expect(callsOf("pty_cwds").length).toBe(before);
  });
});

describe("tab icons", () => {
  const kind = (i: number) => tabs()[i].querySelector<HTMLElement>(".icon")?.dataset.kind;

  test("Claude tabs show the Claude icon and terminal tabs a terminal icon", async () => {
    await startApp();
    await clickClaude();
    await clickTerminal();
    expect([kind(0), kind(1)]).toEqual(["claude", "terminal"]);
    expect(tabs()[1].querySelector(".icon svg")).not.toBeNull();
  });

  test("a terminal tab shows the Claude icon while Claude runs in it", async () => {
    await startApp();
    await clickTerminal();
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(kind(0)).toBe("claude");
    delete openClaudes[1];
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(kind(0)).toBe("terminal");
  });

  const busy = () => tabs().map((t) => t.classList.contains("busy"));
  /** Waits for the clock to pass the current millisecond, so what happens next is later than what came before. */
  const nextMs = () => {
    const now = Date.now();
    while (Date.now() === now);
  };

  test("a tab shows Claude working while its title has a spinner", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("✳ Claude Code");
    expect(busy()).toEqual([false]);
    terms()[0].onTitle!("◐ Claude Code");
    expect(busy()).toEqual([true]);
    terms()[0].onTitle!("◑ Greeting reply");
    expect(busy()).toEqual([true]);
    terms()[0].onTitle!("✳ Greeting reply");
    expect(busy()).toEqual([false]);
  });

  test("a tab shows Claude working while it waits on background tasks", async () => {
    await startApp();
    await clickClaude();
    const tabId = spawns()[0].tabId;
    terms()[0].onTitle!("◐ Claude Code");
    terms()[0].onTitle!("✳ Claude Code");
    backgroundTabs = { [tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([true]);
    expect(kind(0)).toBe("loader");
    expect(tabs()[0].title).toContain("waiting on background tasks");
    backgroundTabs = {};
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
    expect(kind(0)).toBe("claude");
    expect(tabs()[0].title).not.toContain("background");
  });

  test("a turn that ends without reporting, such as an interrupted one, stops the wait on background tasks", async () => {
    await startApp();
    await clickClaude();
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([true]);
    // The tasks woke Claude, and the person interrupted that turn: no Stop hook ran.
    nextMs();
    terms()[0].onTitle!("◐ Claude Code");
    terms()[0].onTitle!("✳ Claude Code");
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
  });

  test("a restarted Claude tab does not show what the Claude before it waited on", async () => {
    await startApp();
    await clickClaude();
    backgroundTabs = { [spawns()[0].tabId]: Date.now() - 1000 };
    spawns()[0].onExit.onmessage(1);
    expect(busy()).toEqual([false]);
    terms()[0].onData!("\r");
    await flush();
    expect(spawns()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
  });

  test("waiting on background tasks shows only while Claude runs in the tab", async () => {
    await startApp();
    await clickTerminal();
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
    // Claude starting in the tab begins after that report, so it is not this Claude's.
    nextMs();
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
    nextMs();
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([true]);
    delete openClaudes[1];
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
    spawns()[0].onExit.onmessage(0);
    expect(busy()).toEqual([false]);
  });

  test("a failed check for background tasks keeps what the tabs showed, and the rest of the check still runs", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    failing.add("background_tabs");
    liveCwds[1] = "/home/me/other";
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([true]);
    expect(tabs()[0].querySelector(".name")!.textContent).toBe("other");
    expect(error).toHaveBeenCalledWith("misag: could not check for background tasks", "background_tabs failed");
  });

  /**
   * The finish sound plays on the first tab check that starts at least
   * FINISH_SETTLE_MS (mirrors tabState.ts) after Claude went idle, so it waits up to a
   * check interval longer; tab checks run every TAB_POLL_MS from boot.
   */
  const FINISH_SETTLE_MS = 1_000;
  const FINISH_SOUND_MAX_MS = FINISH_SETTLE_MS + TAB_POLL_MS;
  /** Runs a turn in the first tab: Claude works, then goes idle. */
  function turn(i = 0) {
    terms()[i].onTitle!("◐ Claude Code");
    terms()[i].onTitle!("✳ Claude Code");
  }

  test("a Claude tab plays the chosen sound once a tab check confirms Claude stayed done", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    turn();
    await vi.advanceTimersByTimeAsync(FINISH_SETTLE_MS - 1);
    expect(sounds.played).toEqual([]);
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS - FINISH_SETTLE_MS + 1);
    expect(sounds.played).toEqual(["chime"]);
    // A title change while idle is not another finish.
    terms()[0].onTitle!("✳ Greeting reply");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS * 2);
    expect(sounds.played).toEqual(["chime"]);
  });

  test("no sound plays when the finish sound is off, including when it is turned off while one is due", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    turn();
    storedSettings = { ...DEFAULTS };
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 3; i++) await flush();
    tabs()[0].click();
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual([]);
    turn();
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual([]);
  });

  test("a turn that starts again soon, or ends waiting on background tasks, has not finished", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "bell" };
    await startApp();
    await clickClaude();
    turn();
    await vi.advanceTimersByTimeAsync(FINISH_SETTLE_MS / 2);
    nextMs();
    terms()[0].onTitle!("◐ Claude Code");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual([]);
    terms()[0].onTitle!("✳ Claude Code");
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS * 2);
    expect(sounds.played).toEqual([]);
    // The tasks woke Claude, and that turn ended waiting on none.
    nextMs();
    terms()[0].onTitle!("◐ Claude Code");
    backgroundTabs = {};
    terms()[0].onTitle!("✳ Claude Code");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual(["bell"]);
  });

  test("a finish waits for a tab check that could read the background tasks", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    storedSettings = { ...DEFAULTS, finishSound: "bell" };
    await startApp();
    await clickClaude();
    failing.add("background_tabs");
    turn();
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS * 2);
    expect(sounds.played).toEqual([]);
    failing.delete("background_tabs");
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(sounds.played).toEqual(["bell"]);
  });

  test("a Claude that exits or quits mid-turn, or another tool's spinner, plays no sound", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "pop" };
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("◐ Claude Code");
    spawns()[0].onExit.onmessage(0);
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    await clickTerminal();
    terms()[1].onTitle!("⠂ Installing");
    terms()[1].onTitle!("done");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    openClaudes[2] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    terms()[1].onTitle!("◐ Fix the build");
    delete openClaudes[2];
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual([]);
  });

  test("restarting a tab whose Claude exited mid-turn plays no sound", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "pop" };
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("◐ Claude Code");
    spawns()[0].onExit.onmessage(1);
    terms()[0].onData!("\r");
    await flush();
    expect(spawns()).toHaveLength(2);
    terms()[0].onTitle!("✳ Claude Code");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS * 2);
    expect(sounds.played).toEqual([]);
  });

  test("a finish while the tab is being renamed still plays", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    terms()[0].onTitle!("◐ Claude Code");
    tabs()[0].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    expect(tabs()[0].querySelector("input")).not.toBeNull();
    terms()[0].onTitle!("✳ Claude Code");
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual(["chime"]);
  });

  test("closing a tab whose finish is due plays no sound", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    turn();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS * 2);
    expect(sounds.played).toEqual([]);
  });

  test("tabs that finish together each play", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "pop" };
    await startApp();
    await clickClaude();
    await clickClaude();
    turn(0);
    turn(1);
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(sounds.played).toEqual(["pop", "pop"]);
  });

  test("a sound that cannot play says so once in a notice", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    sounds.fail = new Error("no audio device");
    await startApp();
    await clickClaude();
    turn();
    await vi.advanceTimersByTimeAsync(FINISH_SOUND_MAX_MS);
    expect(noticeVisible()).toBe(true);
    expect(noticeText()).toContain("finish sound could not play (no audio device)");
  });

  test("a key or click primes audio while a finish sound is chosen", async () => {
    await startApp();
    await clickClaude();
    terms()[0].element.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    expect(sounds.primed).toBe(0);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 3; i++) await flush();
    const select = document.querySelector<HTMLSelectElement>("#settings select[name=finishSound]")!;
    select.value = "chime";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
    tabs()[0].dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    terms()[0].element.dispatchEvent(new KeyboardEvent("keydown", { key: "a", bubbles: true }));
    expect(sounds.primed).toBe(2);
  });

  test("a working Claude tab shows a loader in place of the Claude icon", async () => {
    await startApp();
    await clickClaude();
    renameTab(0, "build");
    terms()[0].onTitle!("◐ Claude Code");
    expect(kind(0)).toBe("loader");
    terms()[0].onTitle!("✳ Claude Code");
    expect(kind(0)).toBe("claude");
  });

  test.each([
    ["◐ a", true],
    ["◓ a", true],
    ["⠁ a", true],
    ["⣿ a", true],
    ["◔ a", false],
    ["⠀ a", false],
    ["◐a", false],
    ["✳ a", false],
  ])("the title %j shows Claude working: %s", async (title, working) => {
    await startApp();
    await clickClaude();
    terms()[0].onTitle!(title);
    expect(busy()).toEqual([working]);
  });

  test("a terminal tab shows Claude working only while Claude runs in it", async () => {
    await startApp();
    await clickTerminal();
    // Another tool's spinner in a plain shell.
    terms()[0].onTitle!("⠂ Installing");
    expect(busy()).toEqual([false]);
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    terms()[0].onTitle!("◐ Fix the build");
    expect(busy()).toEqual([true]);
    // Claude quit mid-turn and the shell set no title.
    delete openClaudes[1];
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(busy()).toEqual([false]);
  });

  test("a renamed or exited tab still stops showing Claude working", async () => {
    await startApp();
    await clickClaude();
    renameTab(0, "build");
    terms()[0].onTitle!("◐ Claude Code");
    expect(tabs()[0].classList.contains("busy")).toBe(true);
    spawns()[0].onExit.onmessage(0);
    expect(tabs()[0].classList.contains("busy")).toBe(false);
  });

  test("a claude that bypassed the wrapper also counts", async () => {
    await startApp();
    await clickTerminal();
    claudeStates[1] = { state: "untracked", pid: 77 };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(kind(0)).toBe("claude");
  });

  test("a check that cannot tell keeps the icon", async () => {
    await startApp();
    await clickTerminal();
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    delete openClaudes[1];
    claudeStates[1] = { state: "unknown" };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(kind(0)).toBe("claude");
  });

  test("an exited terminal tab goes back to the terminal icon", async () => {
    await startApp();
    await clickTerminal();
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    spawns()[0].onExit.onmessage(0);
    expect(kind(0)).toBe("terminal");
  });

  test("a check that returns after the tab exited does not bring the Claude icon back", async () => {
    await startApp();
    await clickTerminal();
    openClaudes[1] = SESSION;
    let release!: () => void;
    claudeCheckGate = new Promise((r) => (release = r));
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    spawns()[0].onExit.onmessage(0);
    release();
    await flush();
    expect(tabs()[0].querySelector<HTMLElement>(".icon")!.dataset.kind).toBe("terminal");
    expect(tabs()[0].title).not.toContain("--dangerously-skip-permissions");
  });

  test("a slow check is not started again until it returns", async () => {
    await startApp();
    await clickTerminal();
    await vi.advanceTimersByTimeAsync(1000);
    let release!: () => void;
    claudeCheckGate = new Promise((r) => (release = r));
    const before = callsOf("claude_open_sessions").length;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS * 3);
    expect(callsOf("claude_open_sessions").length).toBe(before + 1);
    release();
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(callsOf("claude_open_sessions").length).toBe(before + 2);
  });

  test("a Claude started in a terminal tab is saved by the next check", async () => {
    await startApp();
    await clickTerminal();
    await vi.advanceTimersByTimeAsync(1000);
    expect(lastSaved()?.tabs[0].claudeSession).toBeNull();
    openClaudes[1] = SESSION;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS + 500);
    expect(lastSaved()?.tabs[0].claudeSession).toBe(SESSION);
  });

  test("an untracked claude that skips permissions is noted too", async () => {
    await startApp();
    await clickTerminal();
    claudeStates[1] = { state: "untracked", pid: 77, skipsPermissions: true };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
  });

  test("a terminal tab's Claude that skips permissions is noted in the tooltip, and the note goes with it", async () => {
    await startApp();
    await clickTerminal();
    openClaudes[1] = SESSION;
    skippingClaudes.add(1);
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    expect(tabs()[0].querySelector(".badge")).toBeNull();
    delete openClaudes[1];
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabs()[0].title).not.toContain("--dangerously-skip-permissions");
  });
});

describe("process lifecycle", () => {
  test("a failed spawn shows the error in an exited tab", async () => {
    spawnBehaviour = () => {
      throw { code: "cwd_missing", message: "not a directory: /nope" };
    };
    await startApp();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    expect(terms()[0].output).toContain("could not start: not a directory: /nope");
  });

  test("shows the exit code and stops sending input", async () => {
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(3);
    expect(terms()[0].output).toContain("process exited with code 3");
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    terms()[0].onData!("hi\r");
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("Enter restarts an exited tab in place, with the same ids", async () => {
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(1);
    expect(terms()[0].output).toContain("press Enter to restart");
    terms()[0].onData!("x");
    expect(spawns()).toHaveLength(1);
    terms()[0].onData!("\r");
    terms()[0].onData!("\r");
    await flush();
    expect(spawns()).toHaveLength(2);
    const [first, again] = spawns();
    expect(again).toMatchObject({ tabId: first.tabId, claudeSession: first.claudeSession, fallbackToHome: true });
    expect(tabs()[0].classList.contains("exited")).toBe(false);
    expect(tabs()).toHaveLength(1);
    terms()[0].onData!("hi");
    expect(callsOf("pty_write").at(-1)?.args).toMatchObject({ id: 2 });
  });

  test("a tab that could not start can be restarted once the problem is gone", async () => {
    let fail = true;
    spawnBehaviour = (_args, id) => {
      if (fail) throw { code: "failed", message: "cannot check the transcript" };
      return id;
    };
    await startApp();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    fail = false;
    terms()[0].onData!("\r");
    await flush();
    expect(tabs()[0].classList.contains("exited")).toBe(false);
  });

  test("a restarted tab's earlier hook watch stops; the new start has its own", async () => {
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(0);
    terms()[0].onData!("\r");
    await flush();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(callsOf("claude_reported")).toHaveLength(1);
    expect(noticeText()).toContain("has not reported");
  });

  test("an exit that arrives before pty_spawn resolves is not lost", async () => {
    spawnBehaviour = (args, id) => {
      args.onExit.onmessage(0);
      return id;
    };
    await startApp();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
  });
});

describe("input", () => {
  test("text is sent as UTF-8 and binary input as raw bytes", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onData!("é");
    terms()[0].onBinary!(Uint8Array.of(0x80, 0x01));
    expect(callsOf("pty_write").map((c) => c.args.data)).toEqual([[0xc3, 0xa9], [0x80, 0x01]]);
  });

  test("resizes are forwarded to the PTY", async () => {
    await startApp();
    await clickClaude();
    terms()[0].onResize!(100, 30);
    expect(callsOf("pty_resize")[0].args).toMatchObject({ id: 1, cols: 100, rows: 30 });
  });

  describe("middle-click paste", () => {
    const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
    /** Middle-clicks the first terminal; returns whether the press and the release were left to WebKit. */
    async function middleClick(shiftKey = false) {
      const el = terms()[0].element;
      const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 1, shiftKey });
      const up = new MouseEvent("mouseup", { bubbles: true, cancelable: true, button: 1, shiftKey });
      el.dispatchEvent(down);
      el.dispatchEvent(up);
      for (let i = 0; i < 6; i++) await flush();
      return [down.defaultPrevented, up.defaultPrevented];
    }

    test("pastes the primary selection itself, as WebKitGTK 2.54 crashes doing it", async () => {
      primarySelection = "ls -la";
      await startApp();
      await clickClaude();
      expect(await middleClick()).toEqual([true, true]);
      expect(callsOf("primary_selection")).toHaveLength(1);
      expect(typed()).toEqual(["ls -la"]);
    });

    test("pastes in brackets when the program asked for them, without escapes that end them", async () => {
      primarySelection = "a\x1b[201~b";
      await startApp();
      await clickClaude();
      terms()[0].bracketed = true;
      await middleClick();
      expect(typed()).toEqual(["\x1b[200~a[201~b\x1b[201~"]);
    });

    test("an empty or unreadable primary selection types nothing", async () => {
      primarySelection = null;
      await startApp();
      await clickClaude();
      await middleClick();
      failing.add("primary_selection");
      await middleClick();
      expect(typed()).toEqual([]);
    });

    test("a program that tracks the mouse gets the click, and Shift pastes anyway", async () => {
      primarySelection = "ls";
      await startApp();
      await clickClaude();
      terms()[0].tracking = 1000;
      // WebKit's own paste stays off either way.
      expect(await middleClick()).toEqual([true, true]);
      expect(callsOf("primary_selection")).toHaveLength(0);
      await middleClick(true);
      expect(typed()).toEqual(["ls"]);
    });

    test("other buttons are left alone", async () => {
      await startApp();
      await clickClaude();
      const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 });
      terms()[0].element.dispatchEvent(down);
      expect(down.defaultPrevented).toBe(false);
      expect(callsOf("primary_selection")).toHaveLength(0);
    });
  });

  describe("copy on select", () => {
    /** Lets go of the left button over the first terminal, after a selection or not. */
    async function release() {
      terms()[0].element.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
      for (let i = 0; i < 6; i++) await flush();
    }

    test("selecting text copies it, and the selection stays", async () => {
      await startApp();
      await clickClaude();
      const copy = vi.fn(() => true);
      document.execCommand = copy;
      const removed = vi.spyOn(getSelection()!, "removeAllRanges");
      terms()[0].selected = "some output";
      await release();
      expect(copy).toHaveBeenCalledWith("copy");
      expect(removed).not.toHaveBeenCalled();
    });

    test("a click without a selection copies nothing", async () => {
      await startApp();
      await clickClaude();
      const copy = vi.fn(() => true);
      document.execCommand = copy;
      await release();
      expect(copy).not.toHaveBeenCalled();
    });

    test("copying does not also focus the terminal", async () => {
      await startApp();
      await clickClaude();
      document.execCommand = vi.fn(() => true);
      terms()[0].selected = "some output";
      const before = terms()[0].focused;
      await release();
      expect(terms()[0].focused).toBe(before);
    });

    test("a hidden tab's terminal, or one under the settings page, copies nothing", async () => {
      await startApp();
      await clickClaude();
      await clickClaude();
      const copy = vi.fn(() => true);
      document.execCommand = copy;
      terms()[0].selected = "stale output";
      await release();
      expect(copy).not.toHaveBeenCalled();
      document.querySelector<HTMLButtonElement>(".tab")!.click();
      await flush();
      document.querySelector<HTMLButtonElement>("#open-settings")!.click();
      await flush();
      await release();
      expect(copy).not.toHaveBeenCalled();
    });

    test("only the left button copies", async () => {
      await startApp();
      await clickClaude();
      const copy = vi.fn(() => true);
      document.execCommand = copy;
      terms()[0].selected = "some output";
      terms()[0].element.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 2 }));
      for (let i = 0; i < 6; i++) await flush();
      expect(copy).not.toHaveBeenCalled();
    });
  });

  describe("right-click paste", () => {
    const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
    /** Right-clicks the first terminal; returns whether WebKit's menu was kept off. */
    async function rightClick(shiftKey = false) {
      const menu = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2, shiftKey });
      terms()[0].element.dispatchEvent(menu);
      for (let i = 0; i < 6; i++) await flush();
      return menu.defaultPrevented;
    }

    test("pastes the clipboard in place of WebKit's menu, which has no Paste there", async () => {
      clipboard = "git status";
      await startApp();
      await clickClaude();
      expect(await rightClick()).toBe(true);
      expect(callsOf("clipboard_text")).toHaveLength(1);
      expect(typed()).toEqual(["git status"]);
    });

    test("pastes in brackets when the program asked for them, without escapes that end them", async () => {
      clipboard = "a\x1b[201~b";
      await startApp();
      await clickClaude();
      terms()[0].bracketed = true;
      await rightClick();
      expect(typed()).toEqual(["\x1b[200~a[201~b\x1b[201~"]);
    });

    test("an empty or unreadable clipboard types nothing", async () => {
      await startApp();
      await clickClaude();
      await rightClick();
      failing.add("clipboard_text");
      await rightClick();
      expect(typed()).toEqual([]);
      expect(noticeText()).toContain("Could not paste the clipboard");
    });

    test("over selected text it copies the selection instead, and pastes nothing", async () => {
      clipboard = "old";
      await startApp();
      await clickClaude();
      const copy = vi.fn(() => true);
      document.execCommand = copy;
      terms()[0].selected = "some output";
      expect(await rightClick()).toBe(true);
      expect(copy).toHaveBeenCalledWith("copy");
      expect(callsOf("clipboard_text")).toHaveLength(0);
      expect(typed()).toEqual([]);
    });

    test("a program without bracketed paste gets no control characters", async () => {
      clipboard = "a\x1bb\x03c\x9bd\x7fe\tf";
      await startApp();
      await clickClaude();
      await rightClick();
      expect(typed()).toEqual(["abcde\tf"]);
    });

    test("bracketed paste also drops the 8-bit escape that could end it", async () => {
      clipboard = "a\x9b201~b";
      await startApp();
      await clickClaude();
      terms()[0].bracketed = true;
      await rightClick();
      expect(typed()).toEqual(["\x1b[200~a201~b\x1b[201~"]);
    });

    test("more than one line to a program without bracketed paste is pasted only if you agree", async () => {
      clipboard = "rm -rf build\nmake\n";
      await startApp();
      await clickClaude();
      picker.answer = false;
      await rightClick();
      expect(picker.asked).toHaveLength(1);
      expect(typed()).toEqual([]);
      picker.answer = true;
      await rightClick();
      expect(typed()).toEqual(["rm -rf build\rmake\r"]);
    });

    test("a paste dialog that cannot open types nothing and says so", async () => {
      clipboard = "rm -rf build\nmake\n";
      picker.askFails = true;
      const error = vi.spyOn(console, "error").mockImplementation(() => {});
      await startApp();
      await clickClaude();
      await rightClick();
      expect(typed()).toEqual([]);
      expect(noticeText()).toContain("Could not paste the clipboard: no portal.");
      expect(error).toHaveBeenCalledWith("misag: could not ask before pasting the clipboard", expect.any(Error));
    });

    test("a paste still reading the clipboard at dispose types nothing", async () => {
      clipboard = "ls";
      await startApp();
      await clickClaude();
      terms()[0].element.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 }));
      dispose!();
      dispose = null;
      for (let i = 0; i < 6; i++) await flush();
      expect(callsOf("clipboard_text")).toHaveLength(1);
      expect(typed()).toEqual([]);
    });

    test("more than one line in brackets is pasted without asking, as the program will not run it", async () => {
      clipboard = "a\nb";
      await startApp();
      await clickClaude();
      terms()[0].bracketed = true;
      await rightClick();
      expect(picker.asked).toHaveLength(0);
      expect(typed()).toEqual(["\x1b[200~a\rb\x1b[201~"]);
    });

    test("a program that tracks the mouse gets the click, and Shift pastes anyway", async () => {
      clipboard = "ls";
      await startApp();
      await clickClaude();
      terms()[0].tracking = 1000;
      expect(await rightClick()).toBe(true);
      expect(callsOf("clipboard_text")).toHaveLength(0);
      await rightClick(true);
      expect(typed()).toEqual(["ls"]);
    });
  });

  /** Clicks a link in the first terminal, as `detail` 1 (a mouse click) or 0 (the keyboard), and returns the event. */
  async function clickLink(init: MouseEventInit) {
    const link = document.createElement("a");
    link.className = "term-link";
    link.href = "https://tauri.app/";
    terms()[0].element.append(link);
    const click = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
    link.dispatchEvent(click);
    await flush();
    return click;
  }

  test("Ctrl+click opens a link through the opener plugin, not in the app's window", async () => {
    await startApp();
    await clickClaude();
    const click = await clickLink({ ctrlKey: true, detail: 1 });
    expect(click.defaultPrevented).toBe(true);
    expect(callsOf("plugin:opener|open_url")[0].args).toMatchObject({ url: "https://tauri.app/" });
  });

  test("a link activated from the keyboard opens", async () => {
    await startApp();
    await clickClaude();
    await clickLink({ detail: 0 });
    expect(callsOf("plugin:opener|open_url")).toHaveLength(1);
  });

  test("a plain click on a link stays in the terminal", async () => {
    await startApp();
    await clickClaude();
    await clickLink({ detail: 1 });
    expect(callsOf("plugin:opener|open_url")).toHaveLength(0);
  });
});

describe("new terminal dialog", () => {
  test("asks only for a name and directory, with Start as the default button", async () => {
    await startApp();
    expect(field("name")).not.toBeNull();
    expect(field("cwd")).not.toBeNull();
    expect(field("command")).toBeNull();
    expect(dialog().querySelector("button")?.value).toBe("ok");
  });

  test("Shift+click opens it and Start creates the terminal tab", async () => {
    await startApp();
    await clickTerminal(true);
    expect(dialog().open).toBe(true);
    field("name")!.value = "agent";
    field("cwd")!.value = "~/src/misag";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabNames()).toEqual(["agent"]);
    expect(spawns()[0]).toMatchObject({ kind: "terminal", cwd: "~/src/misag" });
  });

  test("can open a Claude tab instead", async () => {
    await startApp();
    await clickTerminal(true);
    dialog().querySelector<HTMLInputElement>("input[name=kind][value=claude]")!.checked = true;
    field("name")!.value = "agent";
    field("cwd")!.value = "/srv/app";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabNames()).toEqual(["agent"]);
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "/srv/app", fallbackToHome: false });
    // The next opening starts at Terminal again.
    await clickTerminal(true);
    expect(dialog().querySelector<HTMLInputElement>("input[name=kind][value=terminal]")!.checked).toBe(true);
  });

  test("Escape after an earlier Start does not spawn", async () => {
    await startApp();
    await clickTerminal(true);
    dialog().close("ok");
    await flush();
    await clickTerminal(true);
    dialog().close();
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_spawn")).toHaveLength(1);
  });
});

describe("restoring tabs", () => {
  const layout: Layout = {
    active: 1,
    tabs: [
      { tabId: "t3", kind: "claude", name: "agent", renamed: true, cwd: "/home/me/src", command: null, claudeSession: SESSION },
      { tabId: "t4", kind: "claude", name: "✳ Review", renamed: false, cwd: null, command: null, claudeSession: null },
      { tabId: "t5", kind: "terminal", name: "logs", renamed: true, cwd: "/var/log", command: "tail -f syslog", claudeSession: null },
      { tabId: "t6", kind: "terminal", name: "shell", renamed: false, cwd: "/home/me/app", command: null, claudeSession: SESSION },
    ],
  };

  test("reopens each tab with its kind, directory, command and session", async () => {
    savedLayout = layout;
    await startApp();
    // An unnamed tab shows its folder, or its title while the folder is unknown.
    expect(tabNames()).toEqual(["agent", "✳ Review", "logs", "app"]);
    const [a, b, c, d] = spawns();
    expect(a).toMatchObject({ tabId: "t3", kind: "claude", cwd: "/home/me/src", claudeSession: SESSION });
    expect(b.kind).toBe("claude");
    expect(b.claudeSession).toMatch(UUID);
    expect(b.tabId).toBe("t4");
    expect(c).toMatchObject({ tabId: "t5", kind: "terminal", cwd: "/var/log", command: "tail -f syslog", claudeSession: null });
    // The backend types `claude --resume` at this shell's prompt.
    expect(d).toMatchObject({ tabId: "t6", kind: "terminal", cwd: "/home/me/app", command: null, claudeSession: SESSION });
    expect(tabs()[1].classList.contains("active")).toBe(true);
    expect(tabs()[2].querySelector<HTMLElement>(".icon")!.dataset.kind).toBe("terminal");
  });

  test("kills leftover sessions before restoring", async () => {
    savedLayout = layout;
    await startApp();
    const order = calls.map((c) => c.cmd);
    expect(order.indexOf("pty_reset")).toBeLessThan(order.indexOf("pty_spawn"));
    expect(order.indexOf("pty_reset")).toBeLessThan(order.indexOf("layout_load"));
  });

  test("a restored tab keeps its id, so its session reports stay attached", async () => {
    savedLayout = layout;
    await startApp();
    await win.closeRequested!();
    expect(lastSaved()?.tabs.map((t) => t.tabId)).toEqual(["t3", "t4", "t5", "t6"]);
  });

  test("restored user names survive titles; others follow them", async () => {
    savedLayout = layout;
    await startApp();
    terms()[0].onTitle!("✳ x");
    terms()[1].onTitle!("✳ y");
    expect(tabNames()).toEqual(["agent", "✳ y", "logs", "app"]);
  });

  test("a vanished directory falls back to the home directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "t8", kind: "claude", name: "old", renamed: true, cwd: "/gone", command: null, claudeSession: SESSION }],
    };
    missingDirs.add("/gone");
    await startApp();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([["/gone", true]]);
    expect(spawns()[0].claudeSession).toBe(SESSION);
    expect(tabs()[0].classList.contains("exited")).toBe(false);
    expect(terms()[0].output).toContain("/gone no longer exists, so “old” started in the home directory.");
  });

  test("with nothing saved, starts with no tabs", async () => {
    savedLayout = { active: 0, tabs: [] };
    await startApp();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
  });
});

describe("saving tabs", () => {
  test("closing the window saves every tab with its live directory and session", async () => {
    await startApp();
    await clickClaude();
    await clickTerminal(true);
    field("name")!.value = "dev";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    liveCwds = { 1: "/home/me/src", 2: "/home/me" };
    // Like the backend, which only reports terminal tabs it was asked about.
    openClaudes = { 1: "6fa459ea-ee8a-4ca4-894e-db77e160355e", 2: SESSION };
    await win.closeRequested!();
    const first = spawns()[0].claudeSession;
    const [firstTab, secondTab] = spawns().map((s) => s.tabId);
    expect(firstTab).not.toBe(secondTab);
    expect(callsOf("claude_open_sessions").at(-1)?.args).toEqual({ tabs: [{ id: 2, tabId: secondTab }] });
    expect(lastSaved()).toEqual({
      active: 1,
      tabs: [
        { tabId: firstTab, kind: "claude", name: "Claude 1", renamed: false, cwd: "/home/me/src", claudeSession: first },
        { tabId: secondTab, kind: "terminal", name: "dev", renamed: true, cwd: "/home/me", command: null, claudeSession: SESSION },
      ],
    });
    // Once Claude has quit in it, the terminal tab is saved without a session.
    openClaudes = {};
    await win.closeRequested!();
    expect(lastSaved()?.tabs[1]).toMatchObject({ kind: "terminal", claudeSession: null });
  });

  test("a restored terminal tab resumes its Claude session once, not on a restart", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "t", kind: "terminal", name: "shell", renamed: false, cwd: null, command: null, claudeSession: SESSION }],
    };
    await startApp();
    spawns()[0].onExit.onmessage(0);
    terms()[0].onData!("\r");
    await flush();
    expect(spawns().map((s) => s.claudeSession)).toEqual([SESSION, null]);
  });

  describe("a terminal tab's Claude session", () => {
    const restoredShell = () => ({
      active: 0,
      tabs: [{ tabId: "t", kind: "terminal" as const, name: "shell", renamed: false, cwd: null, command: null, claudeSession: SESSION }],
    });
    const savedSession = () => lastSaved()?.tabs[0].claudeSession;
    let now: number;
    beforeEach(() => {
      now = 1_000_000;
      vi.spyOn(Date, "now").mockImplementation(() => now);
    });
    afterEach(() => vi.restoreAllMocks());

    test("is kept while the resumed Claude starts, and dropped once it is gone", async () => {
      savedLayout = restoredShell();
      await startApp();
      // Claude has not reported yet: the shell alone is no reason to forget it.
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
      now += 61_000;
      await win.closeRequested!();
      expect(savedSession()).toBeNull();
    });

    test("is kept when the check cannot tell, or the tab never started", async () => {
      spawnBehaviour = () => {
        throw { code: "failed", message: "no pty" };
      };
      savedLayout = restoredShell();
      await startApp();
      now += 61_000;
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
    });

    test("is kept when the check is unknown or fails", async () => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      savedLayout = restoredShell();
      await startApp();
      now += 61_000;
      claudeStates = { 1: { state: "unknown" } };
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
      failing.add("claude_open_sessions");
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
    });

    test("is dropped when its shell exits", async () => {
      savedLayout = restoredShell();
      await startApp();
      spawns()[0].onExit.onmessage(0);
      await win.closeRequested!();
      expect(savedSession()).toBeNull();
    });

    test("without a conversation it is dropped, and a notice says why", async () => {
      resumeReport = { state: "noConversation" };
      savedLayout = restoredShell();
      await startApp();
      expect(noticeText()).toContain("saved no conversation there");
      await win.closeRequested!();
      expect(savedSession()).toBeNull();
    });

    test("a resume that failed is kept for next time, and a notice says why", async () => {
      resumeReport = { state: "failed", message: "could not type the command" };
      savedLayout = restoredShell();
      await startApp();
      expect(noticeText()).toContain("Claude could not be reopened in “shell” (could not type the command)");
      now += 61_000;
      claudeStates = { 1: { state: "unknown" } };
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
    });

    test("starting or quitting Claude is saved within seconds", async () => {
      await startApp();
      await clickTerminal();
      openClaudes = { 1: SESSION };
      await vi.advanceTimersByTimeAsync(6_000);
      expect(savedSession()).toBe(SESSION);
      openClaudes = {};
      await vi.advanceTimersByTimeAsync(6_000);
      expect(savedSession()).toBeNull();
    });

    test("a claude that bypassed the wrapper is reported once it has run a while", async () => {
      await startApp();
      await clickTerminal();
      claudeStates = { 1: { state: "untracked", pid: 77 } };
      await win.closeRequested!();
      expect(noticeText()).not.toContain("wrapper");
      now += 11_000;
      await win.closeRequested!();
      expect(noticeText()).toContain("Claude in “shell 1” did not start through Mischief of Agents's claude wrapper");
      expect(savedSession()).toBeNull();
    });
  });

  test("tabs are still saved when the open Claude sessions cannot be checked", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickTerminal();
    failing.add("claude_open_sessions");
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0]).toMatchObject({ kind: "terminal", claudeSession: null });
    expect(error).toHaveBeenCalledWith("misag: could not check for Claude sessions in terminal tabs", "claude_open_sessions failed");
    error.mockRestore();
  });

  test("closing the last tab saves an empty layout", async () => {
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await win.closeRequested!();
    expect(lastSaved()?.tabs).toEqual([]);
  });

  test("tab changes are saved shortly after they happen", async () => {
    await startApp();
    await openClaudeTabs(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(lastSaved()?.tabs.map((t) => t.name)).toEqual(["Claude 1", "Claude 2"]);
    renameTab(0, "main");
    await vi.advanceTimersByTimeAsync(1000);
    expect(lastSaved()?.tabs[0]).toMatchObject({ name: "main", renamed: true });
  });

  test("a half-finished restore is not saved", async () => {
    savedLayout = {
      active: 0,
      tabs: [
        { tabId: "t11", kind: "claude", name: "a", renamed: true, cwd: null, command: null, claudeSession: null },
        { tabId: "t12", kind: "claude", name: "b", renamed: true, cwd: null, command: null, claudeSession: null },
      ],
    };
    // Each spawn takes longer than the save delay, so an unguarded save would fire mid-restore.
    spawnBehaviour = (_args, id) => new Promise((r) => setTimeout(() => r(id), 1000));
    await startApp();
    await vi.advanceTimersByTimeAsync(5000);
    expect(tabNames()).toEqual(["a", "b"]);
    const saves = callsOf("layout_save").map((c) => (c.args.layout as Layout).tabs.length);
    expect(saves.length).toBeGreaterThan(0);
    expect(saves.every((n) => n === 2)).toBe(true);
  });
});

describe("settings", () => {
  const page = () => document.querySelector<HTMLElement>("#settings")!;
  const pageShown = () => page().classList.contains("active");
  const setting = (name: string) =>
    page().querySelector("form")!.elements.namedItem(name) as HTMLInputElement;
  const pageError = () => page().querySelector<HTMLElement>(".error")!;
  const disabled = () => page().querySelector("fieldset")!.disabled;
  async function openSettings() {
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 3; i++) await flush();
  }
  async function change(name: string, value: boolean) {
    setting(name).checked = value;
    setting(name).dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
  }
  const settingsTabX = () => tabs().at(-1)!.querySelector<HTMLButtonElement>(".x")!;

  test("the sidebar button opens a settings tab over the terminals, and closing it goes back", async () => {
    await startApp();
    await openClaudeTabs(2);
    await openSettings();
    expect(tabNames()).toEqual(["work", "work", "Settings"]);
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([false, false, true]);
    expect(pageShown()).toBe(true);
    expect([...document.querySelectorAll(".term.active")]).toHaveLength(0);
    expect(document.activeElement).toBe(page());
    expect(setting("skipPermissions").checked).toBe(false);

    await openSettings();
    expect(tabNames()).toEqual(["work", "work", "Settings"]);
    tabs()[0].click();
    expect(pageShown()).toBe(false);
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([true, false, false]);
    tabs()[2].click();
    expect(pageShown()).toBe(true);
    settingsTabX().click();
    expect(tabNames()).toEqual(["work", "work"]);
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
  });

  test("with no other tabs, the settings tab opens and closes back to the empty state", async () => {
    await startApp();
    await openSettings();
    expect(tabNames()).toEqual(["Settings"]);
    expect(pageShown()).toBe(true);
    settingsTabX().click();
    expect(tabs()).toHaveLength(0);
    expect(pageShown()).toBe(false);
    expect(noTabs()).toBe(true);
  });

  test("the settings tab is not saved with the tabs", async () => {
    await startApp();
    await openClaudeTabs(1);
    await openSettings();
    await win.closeRequested!();
    expect(lastSaved()!.tabs.map((t) => t.name)).toEqual(["Claude 1"]);
  });

  test("closing the terminal under a shown settings tab keeps settings shown", async () => {
    await startApp();
    await openClaudeTabs(2);
    await openSettings();
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(pageShown()).toBe(true);
    expect(document.querySelectorAll(".term.active")).toHaveLength(0);
    settingsTabX().click();
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
  });

  test("the page shows the stored value each time it opens, and saves changes", async () => {
    await startApp();
    await openSettings();
    expect(setting("skipPermissions").checked).toBe(false);
    await change("skipPermissions", true);
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ ...DEFAULTS, skipPermissions: true });
    expect(pageError().hidden).toBe(true);
    settingsTabX().click();
    // Changed outside the page (another window, or a tab writing the database).
    storedSettings = { ...DEFAULTS };
    await openSettings();
    expect(setting("skipPermissions").checked).toBe(false);
  });

  test("the auto memory select shows the stored value and saves changes either way", async () => {
    const select = () => page().querySelector<HTMLSelectElement>("select[name=disableAutoMemory]")!;
    async function choose(value: "true" | "false") {
      select().value = value;
      select().dispatchEvent(new Event("change", { bubbles: true }));
      for (let i = 0; i < 3; i++) await flush();
    }
    await startApp();
    await openSettings();
    expect(select().value).toBe("false");
    expect(select().selectedOptions[0].textContent).toBe("Auto");
    await choose("true");
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ ...DEFAULTS, disableAutoMemory: true });
    await choose("false");
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual(DEFAULTS);
    settingsTabX().click();
    // Changed outside the page.
    storedSettings = { ...DEFAULTS, disableAutoMemory: true };
    await openSettings();
    expect(select().value).toBe("true");
    expect(select().selectedOptions[0].textContent).toBe("Disable");
  });

  test("changing one Claude setting keeps the other", async () => {
    storedSettings = { ...DEFAULTS, skipPermissions: true };
    await startApp();
    await openSettings();
    const select = page().querySelector<HTMLSelectElement>("select[name=disableAutoMemory]")!;
    select.value = "true";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
    expect(storedSettings).toEqual({ ...DEFAULTS, skipPermissions: true, disableAutoMemory: true });
    await change("skipPermissions", false);
    expect(storedSettings).toEqual({ ...DEFAULTS, disableAutoMemory: true });
  });

  test("the terminal font size is applied at start, shown on the page, and saved and applied when changed", async () => {
    const field = () => setting("fontSize");
    const applied = () => document.documentElement.style.getPropertyValue("--terminal-font-size");
    async function size(value: string) {
      field().value = value;
      field().dispatchEvent(new Event("change", { bubbles: true }));
      for (let i = 0; i < 3; i++) await flush();
    }
    storedSettings = { ...DEFAULTS, fontSize: 16 };
    await startApp();
    expect(applied()).toBe("16px");
    await clickTerminal();
    await openSettings();
    expect(field().value).toBe("16");
    const term = fakes.FakeTerminal.all[0];
    await size("20");
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ ...DEFAULTS, fontSize: 20 });
    expect(applied()).toBe("20px");
    // The terminal measures its cells again once it is shown.
    const before = term.fits;
    tabs()[0].click();
    await vi.advanceTimersByTimeAsync(20);
    expect(term.fits).toBeGreaterThan(before);
  });

  test("a font size out of range is not saved, and says why", async () => {
    await startApp();
    await openSettings();
    const field = setting("fontSize");
    field.value = "40";
    field.dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("settings_save")).toHaveLength(0);
    expect(pageError().textContent).toContain("8 to 32");
    expect(field.value).toBe("14");
    expect(document.documentElement.style.getPropertyValue("--terminal-font-size")).toBe("14px");
  });

  test("choosing a finish sound saves it and plays it, and Off plays nothing", async () => {
    const select = () => page().querySelector<HTMLSelectElement>("select[name=finishSound]")!;
    async function choose(value: string) {
      select().value = value;
      select().dispatchEvent(new Event("change", { bubbles: true }));
      for (let i = 0; i < 3; i++) await flush();
    }
    await startApp();
    await openSettings();
    expect(select().value).toBe("off");
    expect([...select().options].map((o) => o.value)).toEqual(["off", "chime", "bell", "pop"]);
    await choose("bell");
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ ...DEFAULTS, finishSound: "bell" });
    expect(sounds.played).toEqual(["bell"]);
    // Changing another setting does not play it again.
    await change("skipPermissions", true);
    expect(sounds.played).toEqual(["bell"]);
    await choose("off");
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ ...DEFAULTS, skipPermissions: true });
    expect(sounds.played).toEqual(["bell"]);
  });

  test("a finish sound that could not be saved is not played", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await openSettings();
    failing.add("settings_save");
    const select = page().querySelector<HTMLSelectElement>("select[name=finishSound]")!;
    select.value = "pop";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
    expect(sounds.played).toEqual([]);
    expect(select.value).toBe("off");
  });

  test("a finish sound the page does not know is not sent", async () => {
    await startApp();
    await openSettings();
    const select = page().querySelector<HTMLSelectElement>("select[name=finishSound]")!;
    select.append(new Option("Siren", "siren"));
    select.value = "siren";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("settings_save").every((c) => (c.args.settings as Settings).finishSound === "off")).toBe(true);
    expect(sounds.played).toEqual([]);
  });

  test("a missing finish sound shows as Off", async () => {
    storedSettings = { skipPermissions: false, workspace: null, disableAutoMemory: false, fontSize: 14 };
    await startApp();
    await openSettings();
    expect(page().querySelector<HTMLSelectElement>("select[name=finishSound]")!.value).toBe("off");
  });

  test("a missing font size shows as 14", async () => {
    storedSettings = { skipPermissions: false, workspace: null, disableAutoMemory: false };
    await startApp();
    await openSettings();
    expect(setting("fontSize").value).toBe("14");
  });

  test("a missing auto memory value shows as Auto", async () => {
    storedSettings = { skipPermissions: false, workspace: null };
    await startApp();
    await openSettings();
    expect(page().querySelector<HTMLSelectElement>("select[name=disableAutoMemory]")!.value).toBe("false");
  });

  test("the form stays disabled until the stored settings are read", async () => {
    storedSettings = { ...DEFAULTS, skipPermissions: true };
    let release!: () => void;
    settingsLoadGate = new Promise((r) => (release = r));
    await startApp();
    await openSettings();
    expect(disabled()).toBe(true);
    release();
    await flush();
    await flush();
    expect(disabled()).toBe(false);
    expect(setting("skipPermissions").checked).toBe(true);
  });

  test("a failed save is shown and the checkbox goes back to the stored value", async () => {
    await startApp();
    await openSettings();
    failing.add("settings_save");
    await change("skipPermissions", true);
    expect(pageError().hidden).toBe(false);
    expect(pageError().textContent).toContain("settings_save failed");
    expect(setting("skipPermissions").checked).toBe(false);
    failing.delete("settings_save");
    await change("skipPermissions", true);
    expect(pageError().hidden).toBe(true);
    expect(storedSettings.skipPermissions).toBe(true);
  });

  test("saves run in order", async () => {
    await startApp();
    await openSettings();
    await change("skipPermissions", true);
    await change("skipPermissions", false);
    expect(callsOf("settings_save").map((c) => (c.args.settings as Settings).skipPermissions)).toEqual([true, false]);
    expect(storedSettings.skipPermissions).toBe(false);
  });

  test("unreadable settings are shown and cannot be changed", async () => {
    failing.add("settings_load");
    await startApp();
    await openSettings();
    expect(disabled()).toBe(true);
    expect(pageError().textContent).toContain("Claude tabs start with permission prompts and auto memory off");
  });

  test("Claude tabs started with skipped permissions say so in the tooltip, with no badge", async () => {
    storedSettings = { ...DEFAULTS, skipPermissions: true };
    await startApp();
    await openClaudeTabs(1);
    expect(tabs()[0].querySelector(".badge")).toBeNull();
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    storedSettings = { ...DEFAULTS };
    terms()[0].onTitle?.("busy");
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    // The restart picks up the current setting.
    spawns()[0].onExit.onmessage(0);
    terms()[0].onData!("\r");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabs()[0].title).not.toContain("--dangerously-skip-permissions");
  });

  test("without a database the page says settings are not kept", async () => {
    storeReport = { store: { state: "notSaving", reason: "no data dir" }, hookProblem: null };
    await startApp();
    await openSettings();
    expect(page().querySelector<HTMLElement>(".not-saving")!.hidden).toBe(false);
  });
});

describe("file tree", () => {
  const rows = () =>
    [...document.querySelectorAll<HTMLElement>("#workspace-box .tree .row")].map((r) => r.textContent);
  const row = (name: string) =>
    [...document.querySelectorAll<HTMLButtonElement>("#workspace-box .tree button.row")].find((r) =>
      r.textContent!.endsWith(name),
    )!;
  const treeText = () => document.querySelector("#workspace-box .tree")!.textContent;
  const button = (sel: string) => document.querySelector<HTMLButtonElement>(`#workspace-box ${sel}`)!;
  const pageButton = (sel: string) => document.querySelector<HTMLButtonElement>(`#workspace-settings ${sel}`)!;
  const pageError = () => document.querySelector<HTMLElement>("#workspace-settings .error")!;
  const folderField = () => document.querySelector<HTMLInputElement>("#workspace-settings input[name=workspace]")!;
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));

  beforeEach(() => {
    fsTree = {
      "~": [{ name: "notes", dir: true }],
      "/p": [
        { name: "src", dir: true },
        { name: "my notes.txt", dir: false },
      ],
      "/p/src": [{ name: "main.ts", dir: false }],
    };
  });

  test("shows the home directory by default, one level at a time", async () => {
    await startApp();
    await settle();
    expect(rows()).toEqual(["~", "notes"]);
  });

  test("shows the stored folder and does not follow the tabs", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await settle();
    expect(rows()).toEqual(["p", "src", "my notes.txt"]);
    liveCwds = { 1: "/q", 2: "/r" };
    await openClaudeTabs(2);
    tabs()[0].click();
    await settle();
    expect(rows()).toEqual(["p", "src", "my notes.txt"]);
    expect(callsOf("fs_list").map((c) => c.args.dir)).toEqual(["/p"]);
  });

  test("folders are listed when opened, and clicking a file types its quoted path", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await openClaudeTabs(1);
    await settle();
    row("src").click();
    await settle();
    expect(rows()).toEqual(["p", "src", "main.ts", "my notes.txt"]);
    row("main.ts").click();
    row("my notes.txt").click();
    expect(typed()).toEqual(["/p/src/main.ts ", "'/p/my notes.txt' "]);
    row("src").click();
    expect(row("main.ts").closest("ul")!.hidden).toBe(true);
  });

  test("the refresh button reloads and keeps open folders open", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await settle();
    row("src").click();
    await settle();
    fsTree["/p/src"].push({ name: "new.ts", dir: false });
    button(".refresh").click();
    await settle();
    expect(rows()).toEqual(["p", "src", "main.ts", "new.ts", "my notes.txt"]);
  });

  test("the gear opens a workspace settings tab, where the folder is chosen", async () => {
    await startApp();
    await settle();
    button(".configure").click();
    await settle();
    expect(tabNames()).toEqual(["Workspace settings"]);
    expect(document.querySelector("#workspace-settings")!.classList.contains("active")).toBe(true);
    expect(document.querySelector("#settings")!.classList.contains("active")).toBe(false);
    expect(folderField().value).toBe("");
    expect(folderField().placeholder).toBe("~ (your home directory)");

    picker.picked = null;
    folderField().click();
    await settle();
    expect(callsOf("settings_save")).toHaveLength(0);

    picker.picked = "/p";
    key(folderField(), "Enter");
    await settle();
    expect(storedSettings.workspace).toBe("/p");
    expect(folderField().value).toBe("/p");
    expect(rows()).toEqual(["p", "src", "my notes.txt"]);

    pageButton(".home-workspace").click();
    await settle();
    expect(storedSettings.workspace).toBeNull();
    expect(rows()).toEqual(["~", "notes"]);
  });

  test("a folder that is refused or cannot be read is shown", async () => {
    await startApp();
    button(".configure").click();
    await settle();
    picker.picked = "/gone";
    folderField().click();
    await settle();
    expect(pageError().textContent).toContain("/gone is not a folder");
    expect(rows()).toEqual(["~", "notes"]);

    vi.resetModules();
    storedSettings = { ...DEFAULTS, workspace: "/vanished" };
    await startApp();
    await settle();
    expect(treeText()).toContain("cannot read /vanished");
  });

  test("with unreadable settings the tree shows home and the folder cannot be changed", async () => {
    failing.add("settings_load");
    await startApp();
    await settle();
    expect(rows()).toEqual(["~", "notes"]);
    button(".configure").click();
    await settle();
    expect(document.querySelector<HTMLFieldSetElement>("#workspace-settings fieldset")!.disabled).toBe(true);
  });

  test("both pages can be open as their own tabs next to the terminals", async () => {
    await startApp();
    await openClaudeTabs(1);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    button(".configure").click();
    await settle();
    expect(tabNames()).toEqual(["notes", "Settings", "Workspace settings"]);
    const shown = () => [...document.querySelectorAll(".page.active")].map((p) => p.id);
    expect(shown()).toEqual(["workspace-settings"]);
    tabs()[1].click();
    expect(shown()).toEqual(["settings"]);
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(tabNames()).toEqual(["notes", "Workspace settings"]);
    expect(shown()).toEqual([]);
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
    tabs()[1].click();
    expect(shown()).toEqual(["workspace-settings"]);
  });

  test("file names that could act as keys are not typed, and say why", async () => {
    fsTree["/p"].push({ name: "notes\x15curl x|sh\r.txt", dir: false }, { name: "a\\b", dir: false });
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await openClaudeTabs(1);
    await settle();
    row("sh\r.txt").click();
    expect(noticeText()).toContain("control characters");
    row("a\\b").click();
    expect(noticeText()).toContain("backslash");
    expect(callsOf("pty_write")).toHaveLength(0);
    row("my notes.txt").click();
    await settle();
    expect(typed()).toEqual(["'/p/my notes.txt' "]);
    expect(noticeVisible()).toBe(false);
  });

  test("without a running tab, or when typing fails, a notice says so", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await settle();
    row("my notes.txt").click();
    expect(noticeText()).toContain("no tab is running");
    await openClaudeTabs(1);
    failing.add("pty_write");
    row("my notes.txt").click();
    await settle();
    expect(noticeText()).toContain("could not be typed");
  });

  test("clicking a file while a page is shown types it into the tab and shows the tab", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    await openClaudeTabs(1);
    button(".configure").click();
    await settle();
    row("my notes.txt").click();
    await settle();
    expect(typed()).toEqual(["'/p/my notes.txt' "]);
    expect(document.querySelector("#workspace-settings")!.classList.contains("active")).toBe(false);
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
  });

  test("a folder shows Loading until listed, and says when only hidden entries exist", async () => {
    fsTree["/p/empty"] = [];
    fsTree["/p"].unshift({ name: "empty", dir: true });
    fsHidden["/p/empty"] = 2;
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    let release!: () => void;
    fsGates["/p/src"] = new Promise((r) => (release = r));
    await startApp();
    await settle();
    row("src").click();
    await settle();
    expect(treeText()).toContain("Loading…");
    release();
    await settle();
    expect(rows()).toContain("main.ts");
    row("empty").click();
    await settle();
    expect(treeText()).toContain("No visible entries (2 hidden)");
  });

  test("a new workspace starts with its folders closed", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    button(".configure").click();
    await settle();
    row("src").click();
    await settle();
    expect(rows()).toContain("main.ts");
    pageButton(".home-workspace").click();
    await settle();
    picker.picked = "/p";
    folderField().click();
    await settle();
    expect(rows()).toEqual(["p", "src", "my notes.txt"]);
  });

  test("the picker starts in the listed folder, opens on Space, and a failure is shown", async () => {
    await startApp();
    button(".configure").click();
    await settle();
    key(folderField(), " ");
    await settle();
    expect(picker.calls.at(-1)!.defaultPath).toBe("/home/me");
    picker.fails = true;
    folderField().click();
    await settle();
    expect(pageError().textContent).toContain("folder picker could not be opened");
  });

  test("saving one setting keeps the other", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await startApp();
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    await settle();
    const skip = document.querySelector<HTMLInputElement>("#settings input[name=skipPermissions]")!;
    skip.checked = true;
    skip.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(storedSettings).toEqual({ ...DEFAULTS, skipPermissions: true, workspace: "/p" });
    button(".configure").click();
    await settle();
    pageButton(".home-workspace").click();
    await settle();
    expect(storedSettings).toEqual({ ...DEFAULTS, skipPermissions: true });
  });

  test("a workspace folder that has gone does not block other settings", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/vanished" };
    await startApp();
    await settle();
    expect(treeText()).toContain("cannot read /vanished");
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    await settle();
    const skip = document.querySelector<HTMLInputElement>("#settings input[name=skipPermissions]")!;
    skip.checked = true;
    skip.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(storedSettings.skipPermissions).toBe(true);
    expect(document.querySelector<HTMLElement>("#settings .error")!.hidden).toBe(true);
  });

  test("unreadable settings at startup are shown, with what that means for each page", async () => {
    failing.add("settings_load");
    await startApp();
    await settle();
    expect(noticeText()).toContain("the Workspace box shows your home directory");
    expect(noticeText()).toContain("Claude tabs start with permission prompts and auto memory off");
    button(".configure").click();
    await settle();
    expect(pageError().textContent).toContain("its folder cannot be changed");
  });

  test("every icon in the page is drawn", async () => {
    await startApp();
    const icons = [...document.querySelectorAll("[data-icon]")];
    expect(icons.length).toBeGreaterThan(5);
    for (const el of icons) expect(el.querySelector("svg"), (el as HTMLElement).dataset.icon).not.toBeNull();
  });

  test("the toolbox collapses and remembers it", async () => {
    await startApp();
    const box = () => document.querySelector("#workspace-box")!;
    button(".toggle").click();
    expect(box().classList.contains("collapsed")).toBe(true);
    // A fresh app instance on a fresh page.
    vi.resetModules();
    await startApp();
    expect(box().classList.contains("collapsed")).toBe(true);
    vi.resetModules();
    localStorage.clear();
    await startApp();
    expect(box().classList.contains("collapsed")).toBe(false);
  });
});

describe("switching and layout", () => {
  test("clicking a tab makes it and its terminal active", async () => {
    await startApp();
    await openClaudeTabs(2);
    tabs()[0].click();
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([true, false]);
    const panes = [...document.querySelectorAll(".term")];
    expect(panes.map((p) => p.classList.contains("active"))).toEqual([true, false]);
  });

  test("the sidebar toggle collapses it and remembers the choice", async () => {
    await startApp();
    const collapsed = () => document.body.classList.contains("sidebar-collapsed");
    expect(collapsed()).toBe(false);
    document.querySelector<HTMLButtonElement>("#toggle-sidebar")!.click();
    expect(collapsed()).toBe(true);

    // A fresh app instance on a fresh page.
    vi.resetModules();
    document.body.className = "";
    await startApp();
    expect(collapsed()).toBe(true);
  });

  test("a restored active index past the end selects the last tab", async () => {
    savedLayout = {
      active: 9,
      tabs: [
        { tabId: "a", kind: "claude", name: "a", renamed: true, cwd: null, command: null, claudeSession: null },
        { tabId: "b", kind: "claude", name: "b", renamed: true, cwd: null, command: null, claudeSession: null },
      ],
    };
    await startApp();
    expect(tabs()[1].classList.contains("active")).toBe(true);
  });
});

describe("new tab directory", () => {
  test("falls back to the active tab's saved directory when its live one is unknown", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: null }],
    };
    await startApp();
    await clickTerminal();
    expect(spawns()[1].cwd).toBe("/srv/app");
  });

  test("still opens a tab when the directory lookup fails", async () => {
    await startApp();
    await clickTerminal();
    failing.add("pty_cwds");
    await clickTerminal();
    expect(tabs()).toHaveLength(2);
    expect(spawns()[1].cwd).toBeNull();
  });

  test("the dialog uses the current tab's directory when left empty", async () => {
    await startApp();
    await clickClaude();
    liveCwds = { 1: "/home/me/x" };
    await clickTerminal(true);
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns()[1].cwd).toBe("/home/me/x");
  });
});

describe("failures", () => {
  test("an unreadable saved layout starts empty and is never overwritten", async () => {
    failing.add("layout_load");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
    expect(noticeText()).toContain("Saved tabs could not be read");
    await clickClaude();
    await vi.advanceTimersByTimeAsync(1000);
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
    error.mockRestore();
  });

  test("a failed session reset still restores the saved tabs", async () => {
    failing.add("pty_reset");
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: null, command: null, claudeSession: null }],
    };
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    expect(tabNames()).toEqual(["a"]);
    await win.closeRequested!();
    expect(lastSaved()?.tabs.map((t) => t.tabId)).toEqual(["a"]);
    error.mockRestore();
  });

  test("closing the window while tabs are still restoring keeps the saved layout", async () => {
    savedLayout = {
      active: 0,
      tabs: [
        { tabId: "a", kind: "claude", name: "a", renamed: true, cwd: null, command: null, claudeSession: null },
        { tabId: "b", kind: "claude", name: "b", renamed: true, cwd: null, command: null, claudeSession: null },
      ],
    };
    spawnBehaviour = (_args, id) => new Promise((r) => setTimeout(() => r(id), 1000));
    await startApp();
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
  });

  test("a failed directory lookup still saves, with each tab's original directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: null }],
    };
    await startApp();
    failing.add("pty_cwds");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0].cwd).toBe("/srv/app");
    error.mockRestore();
  });

  test("other spawn errors do not retry in the home directory or forget the directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: null }],
    };
    spawnBehaviour = () => {
      throw { code: "failed", message: "claude: command not found" };
    };
    await startApp();
    expect(spawns().map((s) => s.cwd)).toEqual(["/srv/app"]);
    expect(terms()[0].output).toContain("could not start: claude: command not found");
    expect(terms()[0].output).not.toContain("home directory");
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0].cwd).toBe("/srv/app");
  });

  test("a vanished directory that also fails elsewhere keeps the saved directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/gone", command: null, claudeSession: null }],
    };
    missingDirs.add("/gone");
    spawnBehaviour = () => {
      throw { code: "failed", message: "no pty" };
    };
    await startApp();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0].cwd).toBe("/gone");
  });

  test("a failed save shows a notice that clears after the next good save", async () => {
    await startApp();
    await clickClaude();
    failing.add("layout_save");
    await win.closeRequested!();
    expect(noticeText()).toContain("Could not save tabs: layout_save failed");
    failing.delete("layout_save");
    await win.closeRequested!();
    expect(noticeVisible()).toBe(false);
  });

  test("the backend's store status is shown, and only dismissible notices can be dismissed", async () => {
    storeReport.store = { state: "notSaving", reason: "the database was written by a newer Mischief of Agents" };
    await startApp();
    expect(noticeText()).toContain("Tabs are not being saved: the database was written by a newer Mischief of Agents.");
    expect(dismissButton().hidden).toBe(true);
  });

  test("a recovery notice can be dismissed and stays dismissed", async () => {
    storeReport.store = { state: "recovered", notice: "Saved tabs could not be read, so Mischief of Agents started fresh." };
    await startApp();
    expect(noticeVisible()).toBe(true);
    dismissButton().click();
    expect(noticeVisible()).toBe(false);
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
  });

  test("a dismissed hook failure stays hidden until a new one is logged", async () => {
    storeReport.hookProblem = { at: 2000, message: "first failure", log: LOG };
    await startApp();
    expect(noticeText()).toContain("first failure");
    dismissButton().click();
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
    storeReport.hookProblem = { at: 2100, message: "first failure", log: LOG };
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).toContain("first failure");
    storeReport.hookProblem = null;
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
  });

  test("a failed save on close cancels the first close with a warning, and the second close quits", async () => {
    await startApp();
    await clickClaude();
    failing.add("layout_save");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await expect(win.closeRequested!()).resolves.not.toThrow();
    expect(error).toHaveBeenCalledWith("misag: could not save tabs", "layout_save failed");
    expect(win.prevented).toBe(true);
    expect(noticeText()).toContain("Close the window again to quit without saving.");
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
    error.mockRestore();
  });

  test("a close after a good save is not cancelled", async () => {
    await startApp();
    await clickClaude();
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
  });

  test("a failed save is tried again until it works", async () => {
    await startApp();
    failing.add("layout_save");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await clickClaude();
    await vi.advanceTimersByTimeAsync(1000);
    expect(noticeText()).toContain("Could not save tabs");
    const tries = callsOf("layout_save").length;
    failing.delete("layout_save");
    await vi.advanceTimersByTimeAsync(6000);
    expect(callsOf("layout_save").length).toBeGreaterThan(tries);
    expect(lastSaved()?.tabs).toHaveLength(1);
    expect(noticeVisible()).toBe(false);
    error.mockRestore();
  });

  test("an exit that arrives after the tab was closed is ignored", async () => {
    await startApp();
    await clickClaude();
    const { onExit } = spawns()[0];
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    const before = terms()[0].written.length;
    onExit.onmessage(0);
    expect(terms()[0].written).toHaveLength(before);
    expect(tabs()).toHaveLength(0);
  });

  test("typing before the process has started is dropped, not sent to a missing id", async () => {
    spawnBehaviour = () => new Promise(() => {});
    await startApp();
    await clickClaude();
    terms()[0].onData!("early");
    terms()[0].onResize!(90, 20);
    expect(callsOf("pty_write")).toHaveLength(0);
    expect(callsOf("pty_resize")).toHaveLength(0);
  });

  test("a tab closed while its process starts has the process killed once it has", async () => {
    let started!: (id: number) => void;
    spawnBehaviour = () => new Promise((r) => (started = r));
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    started(7);
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_kill").map((c) => c.args)).toEqual([{ id: 7 }]);
  });

  test("a tab closed while it restarts has the new process killed once it has started", async () => {
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(0);
    let started!: (id: number) => void;
    spawnBehaviour = () => new Promise((r) => (started = r));
    terms()[0].onData!("\r");
    expect(spawns()).toHaveLength(2);
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    started(8);
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_kill").map((c) => c.args)).toEqual([{ id: 8 }]);
  });

  test("a tab closed while its process fails to start only logs the failure", async () => {
    let failed!: (err: unknown) => void;
    spawnBehaviour = () => new Promise((_r, reject) => (failed = reject));
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    const before = terms()[0].written.length;
    failed("no pty");
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_kill")).toHaveLength(0);
    expect(terms()[0].written).toHaveLength(before);
    expect(error).toHaveBeenCalledWith("misag: could not start the process of a closed tab", "no pty");
    error.mockRestore();
  });

  test("a failed kill of a tab closed while its process starts is only logged", async () => {
    let started!: (id: number) => void;
    spawnBehaviour = () => new Promise((r) => (started = r));
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    failing.add("pty_kill");
    started(7);
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_kill").map((c) => c.args)).toEqual([{ id: 7 }]);
    expect(error).toHaveBeenCalledWith("misag: could not kill process 7 of a closed tab", "pty_kill failed");
    error.mockRestore();
  });

  test("a tab left open while its process starts gets the process, and closing it later kills it", async () => {
    let started!: (id: number) => void;
    spawnBehaviour = () => new Promise((r) => (started = r));
    await startApp();
    await clickClaude();
    started(7);
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_kill")).toHaveLength(0);
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    expect(callsOf("pty_kill").map((c) => c.args)).toEqual([{ id: 7 }]);
  });

  test("a restored tab closed while it starts in the home directory leaves no notice behind", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "t8", kind: "claude", name: "old", renamed: true, cwd: "/gone", command: null, claudeSession: SESSION }],
    };
    missingDirs.add("/gone");
    let started!: (id: number) => void;
    spawnBehaviour = () => new Promise((r) => (started = r));
    await startApp();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    started(9);
    for (let i = 0; i < 3; i++) await flush();
    expect(noticeText()).not.toContain("no longer exists");
    expect(callsOf("pty_kill").map((c) => c.args)).toEqual([{ id: 9 }]);
  });
});

describe("terminal views", () => {
  test("a tab closed while its terminal loads starts no process", async () => {
    fakes.FakeTerminal.hold = true;
    await startApp();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    terms()[0].loading!.resolve();
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_spawn")).toHaveLength(0);
    expect(tabs()).toHaveLength(0);
  });

  test("a terminal that fails to load after its tab was closed leaves the other tabs alone", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    fakes.FakeTerminal.hold = true;
    await clickClaude();
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    terms()[1].loading!.reject(new Error("no wasm"));
    for (let i = 0; i < 3; i++) await flush();
    expect(tabs()).toHaveLength(1);
    expect(terms()[0].destroyed).toBe(false);
    expect(noticeText()).not.toContain("could not be opened");
    error.mockRestore();
  });

  test("a tab that cannot be opened gives the screen back to the tab shown before", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    fakes.FakeTerminal.failOnce = true;
    await clickClaude();
    expect(tabs()).toHaveLength(1);
    expect(tabs()[0].classList.contains("active")).toBe(true);
    expect(terms()[0].element.classList.contains("active")).toBe(true);
    expect(noticeText()).toContain("A tab could not be opened");
    error.mockRestore();
  });

  test("a new tab's terminal is focused once it has loaded", async () => {
    fakes.FakeTerminal.hold = true;
    await startApp();
    await clickClaude();
    terms()[0].loading!.resolve();
    for (let i = 0; i < 3; i++) await flush();
    expect(terms()[0].focused).toBeGreaterThan(0);
  });

  test("switching tabs focuses the shown terminal at once, without the tab taking focus first", async () => {
    await startApp();
    await openClaudeTabs(2);
    await flush();
    const before = terms()[0].focused;
    const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 });
    tabs()[0].dispatchEvent(down);
    document.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
    tabs()[0].click();
    expect(down.defaultPrevented).toBe(true);
    expect(terms()[0].focused).toBeGreaterThan(before);
  });

  test("letting go of a click anywhere on the terminal focuses it, even when its rows were redrawn meanwhile", async () => {
    await startApp();
    await clickClaude();
    await flush();
    const before = terms()[0].focused;
    const row = document.createElement("div");
    terms()[0].element.append(row);
    row.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
    await flush();
    expect(terms()[0].focused).toBe(before + 1);
  });

  test("letting go of the right button, or over text wterm holds selected, does not focus", async () => {
    await startApp();
    await clickClaude();
    await flush();
    const before = terms()[0].focused;
    const el = terms()[0].element;
    el.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 2 }));
    terms()[0].selected = "all of it";
    el.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
    await flush();
    expect(terms()[0].focused).toBe(before);
  });

  test("pressing a tab's close button or rename field is left to them", async () => {
    await startApp();
    await clickClaude();
    for (const target of [tabs()[0].querySelector(".x")!, tabs()[0].querySelector(".name")!.appendChild(document.createElement("input"))]) {
      const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 });
      target.dispatchEvent(down);
      document.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
      expect(down.defaultPrevented).toBe(false);
    }
  });

  test("letting go of a drag that selected text keeps the selection", async () => {
    await startApp();
    await clickClaude();
    await flush();
    const before = terms()[0].focused;
    const row = document.createElement("div");
    row.textContent = "selected text";
    terms()[0].element.append(row);
    getSelection()!.selectAllChildren(row);
    row.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
    await flush();
    expect(terms()[0].focused).toBe(before);
    getSelection()!.removeAllRanges();
  });

  test("a terminal fills its pane, rather than keeping the height of the rows it started with", async () => {
    await startApp();
    await clickClaude();
    for (let i = 0; i < 3; i++) await flush();
    expect(terms()[0].element.style.height).toBe("");
  });

  test("only the shown tab's terminal paints", async () => {
    await startApp();
    await openClaudeTabs(2);
    expect(terms().map((t) => t.paused)).toEqual([true, false]);
    tabs()[0].click();
    expect(terms().map((t) => t.paused)).toEqual([false, true]);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    expect(terms().map((t) => t.paused)).toEqual([true, true]);
  });

  test("an error thrown while writing to a terminal does not stop a restart", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await clickClaude();
    spawns()[0].onExit.onmessage(0);
    vi.spyOn(terms()[0], "write").mockImplementation(() => {
      throw new Error("callback failed");
    });
    terms()[0].onData!("\r");
    for (let i = 0; i < 3; i++) await flush();
    expect(callsOf("pty_spawn")).toHaveLength(2);
    expect(tabs()[0].classList.contains("exited")).toBe(false);
    error.mockRestore();
  });

  test("the window's CSP lets the terminal compile its WebAssembly, and nothing else is eval'd", () => {
    const csp: string = JSON.parse(tauriConfig).app.security.csp;
    const scriptSrc = csp.split(";").find((d) => d.trim().startsWith("script-src"))!;
    expect(scriptSrc).toContain("'wasm-unsafe-eval'");
    expect(scriptSrc).not.toContain("'unsafe-eval'");
  });
});

describe("edge cases", () => {
  test("an unchanged layout is not written again", async () => {
    await startApp();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(1000);
    const saves = callsOf("layout_save").length;
    terms()[0].onTitle!("Claude 1");
    await vi.advanceTimersByTimeAsync(1000);
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(saves);
    renameTab(0, "renamed");
    await vi.advanceTimersByTimeAsync(1000);
    expect(callsOf("layout_save")).toHaveLength(saves + 1);
  });

  test("switching tabs saves which one is active", async () => {
    await startApp();
    await openClaudeTabs(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(lastSaved()?.active).toBe(1);
    tabs()[0].click();
    await vi.advanceTimersByTimeAsync(1000);
    expect(lastSaved()?.active).toBe(0);
  });

  test("an exited tab is saved with the directory it was opened in", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: SESSION }],
    };
    await startApp();
    liveCwds = { 1: "/elsewhere" };
    spawns()[0].onExit.onmessage(0);
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0].cwd).toBe("/srv/app");
  });

  test("a tab whose view cannot be created switches saving off", async () => {
    savedLayout = {
      active: 0,
      tabs: [
        { tabId: "a", kind: "claude", name: "a", renamed: true, cwd: null, command: null, claudeSession: null },
        { tabId: "b", kind: "claude", name: "b", renamed: true, cwd: null, command: null, claudeSession: null },
      ],
    };
    fakes.FakeTerminal.failOnce = true;
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    expect(tabNames()).toEqual(["b"]);
    expect(document.querySelectorAll(".term")).toHaveLength(1);
    expect(noticeText()).toContain("A tab could not be opened: wterm: failed to initialize: no wasm.");
    expect(noticeText()).toContain("1 saved tab(s) could not be reopened");
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
    error.mockRestore();
  });

  test("a restore that fails outright switches saving off", async () => {
    savedLayout = { active: 0, tabs: null } as unknown as Layout;
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    expect(noticeText()).toContain("Saved tabs could not be restored");
    await clickClaude();
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
    error.mockRestore();
  });

  test("a failed session reset is shown", async () => {
    failing.add("pty_reset");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    expect(noticeText()).toContain("Leftover sessions could not be stopped");
    error.mockRestore();
  });

  test("hook failures logged by the backend are picked up by the status check", async () => {
    await startApp();
    expect(noticeVisible()).toBe(false);
    storeReport.hookProblem = { at: 2000, message: "schema 2 in misag.db, this binary knows 1", log: LOG };
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).toContain("Claude session tracking failed (schema 2 in misag.db");
    expect(noticeText()).toContain(`see ${LOG}.`);
  });

  test("a Claude tab whose hook never reports gets a warning", async () => {
    await startApp();
    await clickClaude();
    await clickClaude();
    reported.add(spawns()[1].tabId);
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("Claude in “Claude 1” has not reported its session");
    expect(noticeText()).not.toContain("Claude 2");
    expect(callsOf("claude_reported")[0].args).toMatchObject({ since: 1000 });
  });

  test("the warning clears once the hook reports, or when the tab closes", async () => {
    await startApp();
    await clickClaude();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("Claude 1");
    expect(noticeText()).toContain("Claude 2");
    reported.add(spawns()[0].tabId);
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).not.toContain("Claude 1");
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(noticeVisible()).toBe(false);
  });

  test("a dismissed hook warning stays hidden when Claude retitles the tab", async () => {
    await startApp();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("has not reported");
    dismissButton().click();
    terms()[0].onTitle!("✳ Fix the build");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
    terms()[0].onTitle!("✳ Something else");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
  });

  test("a failing report check neither warns nor gives up", async () => {
    await startApp();
    await clickClaude();
    failing.add("claude_reported");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).not.toContain("has not reported");
    expect(error).toHaveBeenCalledWith("misag: could not check the session report", "claude_reported failed");
    failing.delete("claude_reported");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).toContain("has not reported");
    error.mockRestore();
  });

  test("the warning goes away when the tab's process exits", async () => {
    await startApp();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("has not reported");
    spawns()[0].onExit.onmessage(0);
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
    expect(callsOf("claude_reported")).toHaveLength(1);
  });

  test("untracked Claude tabs get no hook warning", async () => {
    storeReport.store = { state: "notSaving", reason: "no data directory" };
    await startApp();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).not.toContain("has not reported");
    expect(callsOf("claude_reported")).toHaveLength(0);
  });

  test("starting in the home directory is also shown as a notice", async () => {
    await startApp();
    await clickTerminal();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickTerminal();
    expect(noticeText()).toContain("/gone no longer exists, so “shell 2” started in the home directory.");
  });

  test("a new tab from a vanished active directory starts in the home directory", async () => {
    await startApp();
    await clickTerminal();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickTerminal();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([[null, true], ["/gone", true]]);
    expect(tabs()[1].classList.contains("exited")).toBe(false);
  });

  test("a directory typed into the dialog does not fall back silently", async () => {
    missingDirs.add("/typo");
    await startApp();
    await clickTerminal(true);
    field("cwd")!.value = "/typo";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([["/typo", false]]);
    expect(tabs()[0].classList.contains("exited")).toBe(true);
  });

  test("a claude wrapper that could not be installed is reported", async () => {
    storeReport.wrapperProblem = "cannot write the claude wrapper in /d/bin: Read-only file system";
    await startApp();
    expect(noticeText()).toContain(
      "Claude sessions started in terminal tabs will not be reopened: cannot write the claude wrapper in /d/bin",
    );
  });

  test("a misag shim that could not be installed is reported", async () => {
    storeReport.cliProblem = "cannot write the misag shim in /d/cli: Read-only file system";
    await startApp();
    expect(noticeText()).toContain("misag flow is not available in tabs: cannot write the misag shim in /d/cli");
  });

  test("restores the layout fixture the Rust tests also parse", async () => {
    savedLayout = JSON.parse(sharedLayout);
    await startApp();
    expect(tabNames()).toEqual(["agent", "logs", "app"]);
    expect(spawns()[0]).toMatchObject({ kind: "claude", tabId: "t1", claudeSession: SESSION, cwd: "/home/me/src" });
    expect(spawns()[1]).toMatchObject({ kind: "terminal", tabId: "t2", command: "tail -f syslog", claudeSession: null });
    expect(spawns()[2]).toMatchObject({ kind: "terminal", tabId: "t3", command: null, claudeSession: SESSION });
    // Still open in the restored shell at the next save.
    openClaudes = { 3: SESSION };
    await win.closeRequested!();
    expect(lastSaved()).toEqual(JSON.parse(sharedLayout));
  });

  test("the window may be destroyed after the close-time save", () => {
    expect(JSON.parse(capabilities).permissions).toContain("core:window:allow-destroy");
  });
});

describe("flows", () => {
  const box = () => document.querySelector<HTMLElement>("#flows-box")!;
  const boxRows = () => [...box().querySelectorAll<HTMLElement>(".flows li .name")].map((n) => n.textContent);
  const boxText = () => box().querySelector(".body")!.textContent ?? "";
  const page = () => document.querySelector<HTMLElement>("#flows-settings")!;
  const pageShown = () => page().classList.contains("active");
  const pageError = () => page().querySelector<HTMLElement>(".error")!;
  const listed = (list: "flow-list" | "archived-list") =>
    [...page().querySelectorAll<HTMLElement>(`.${list} li`)];
  const listedNames = (list: "flow-list" | "archived-list") =>
    listed(list).map((li) => li.querySelector<HTMLInputElement>("input.name")?.value ?? li.querySelector(".name")?.textContent);
  const rowOf = (name: string) =>
    [...page().querySelectorAll<HTMLElement>("li")].find(
      (li) => (li.querySelector<HTMLInputElement>("input.name")?.value ?? li.querySelector(".name")?.textContent) === name,
    )!;
  const settle = async () => {
    for (let i = 0; i < 5; i++) await flush();
  };
  async function openFlows() {
    box().querySelector<HTMLButtonElement>(".configure")!.click();
    await settle();
  }
  async function create(name: string) {
    const form = page().querySelector<HTMLFormElement>("form.new-flow")!;
    form.querySelector<HTMLInputElement>("input[name=name]")!.value = name;
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await settle();
  }
  async function click(name: string, action: string) {
    rowOf(name).querySelector<HTMLButtonElement>(`button.${action}`)!.click();
    await settle();
  }
  const flows = (...names: string[]) => names.map((name, i) => ({ id: i + 1, name, archived: false }));

  test("Import adds a flow from the file picked and opens it", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    page().querySelector<HTMLButtonElement>("button.import")!.click();
    await settle();
    // The backend asks for the file, so the page never names one.
    expect(callsOf("flow_import_file").map((c) => c.args)).toEqual([{}]);
    expect(listedNames("flow-list")).toEqual(["Deploy", "Imported"]);
    expect(boxRows()).toEqual(["Deploy", "Imported"]);
    expect(tabNames()).toEqual(["Flows", "Imported"]);
  });

  test("a cancelled import does nothing, and a refused one says why", async () => {
    flowPicked = false;
    await startApp();
    await openFlows();
    page().querySelector<HTMLButtonElement>("button.import")!.click();
    await settle();
    expect(tabNames()).toEqual(["Flows"]);
    expect(pageError().hidden).toBe(true);
    flowPicked = true;
    importRefusal = "not a flow file: it has no \"misagFlow\": 1";
    page().querySelector<HTMLButtonElement>("button.import")!.click();
    await settle();
    expect(pageError().textContent).toContain("not a flow file");
    expect(listedNames("flow-list")).toEqual([]);
  });

  test("Export saves a flow where the person picks, or says why it could not", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    await click("Deploy", "export");
    expect(callsOf("flow_export_file").map((c) => c.args)).toEqual([{ id: 1 }]);
    expect(pageError().hidden).toBe(true);
    importRefusal = "cannot write /x/deploy.json: Permission denied";
    await click("Deploy", "export");
    expect(pageError().textContent).toBe('Could not export "Deploy": cannot write /x/deploy.json: Permission denied');
  });

  test("an archived flow can be exported too", async () => {
    storedFlows = flows("Old");
    storedFlows[0].archived = true;
    await startApp();
    await openFlows();
    await click("Old", "export");
    expect(callsOf("flow_export_file").map((c) => c.args)).toEqual([{ id: 1 }]);
  });

  test("a flow list that cannot be read on a poll leaves the flows working", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    failing.add("flows_list");
    await vi.advanceTimersByTimeAsync(2_000);
    await settle();
    failing.delete("flows_list");
    await create("Review");
    expect(listedNames("flow-list")).toEqual(["Deploy", "Review"]);
  });

  test("a flow added outside the app, as by misag flow import, shows up", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openClaudeTabs(1);
    storedFlows.push({ id: 2, name: "From Claude", archived: false });
    await vi.advanceTimersByTimeAsync(2_000);
    await settle();
    expect(boxRows()).toEqual(["Deploy", "From Claude"]);
  });

  test("the toolbox lists the flows that are not archived, in order", async () => {
    storedFlows = flows("Deploy", "Old", "Review");
    storedFlows[1].archived = true;
    await startApp();
    await settle();
    expect(boxRows()).toEqual(["Deploy", "Review"]);
  });

  test("with no flows the toolbox says how to add one", async () => {
    await startApp();
    await settle();
    expect(boxRows()).toEqual([]);
    expect(boxText()).toContain("No flows yet");
  });

  test("a toolbox that cannot read the flows says so", async () => {
    failing.add("flows_list");
    await startApp();
    await settle();
    expect(boxText()).toContain("flows_list failed");
  });

  test("the toolbox collapses on its own and remembers it", async () => {
    await startApp();
    box().querySelector<HTMLButtonElement>(".toggle")!.click();
    expect(box().classList.contains("collapsed")).toBe(true);
    expect(document.querySelector("#workspace-box")!.classList.contains("collapsed")).toBe(false);
    vi.resetModules();
    await startApp();
    expect(box().classList.contains("collapsed")).toBe(true);
  });

  test("the gear opens one Flows tab, which is not saved with the tabs", async () => {
    await startApp();
    await openClaudeTabs(1);
    await openFlows();
    await openFlows();
    expect(tabNames()).toEqual(["work", "Flows"]);
    expect(pageShown()).toBe(true);
    await win.closeRequested!();
    expect(lastSaved()!.tabs.map((t) => t.name)).toEqual(["Claude 1"]);
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(pageShown()).toBe(false);
  });

  test("the page lists active and archived flows apart", async () => {
    storedFlows = flows("Deploy", "Old", "Review");
    storedFlows[1].archived = true;
    await startApp();
    await openFlows();
    expect(listedNames("flow-list")).toEqual(["Deploy", "Review"]);
    expect(listedNames("archived-list")).toEqual(["Old"]);
  });

  test("a new flow is created by name and shows up in the toolbox", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    await create("  Review ");
    expect(callsOf("flow_create").map((c) => c.args.name)).toEqual(["  Review "]);
    expect(listedNames("flow-list")).toEqual(["Deploy", "Review"]);
    expect(boxRows()).toEqual(["Deploy", "Review"]);
    expect(page().querySelector<HTMLInputElement>("form.new-flow input[name=name]")!.value).toBe("");
  });

  test("a flow that cannot be created shows why and keeps the name", async () => {
    await startApp();
    await openFlows();
    await create("   ");
    expect(pageError().hidden).toBe(false);
    expect(pageError().textContent).toContain("a flow needs a name");
    expect(page().querySelector<HTMLInputElement>("form.new-flow input[name=name]")!.value).toBe("   ");
    await create("Deploy");
    expect(pageError().hidden).toBe(true);
  });

  test("renaming a flow saves it and updates the toolbox", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    const input = rowOf("Deploy").querySelector<HTMLInputElement>("input.name")!;
    input.value = "Ship";
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(callsOf("flow_rename").map((c) => c.args)).toEqual([{ id: 1, name: "Ship" }]);
    expect(boxRows()).toEqual(["Ship"]);
  });

  test("a rejected rename goes back to the stored name", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    const input = rowOf("Deploy").querySelector<HTMLInputElement>("input.name")!;
    input.value = " ";
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(pageError().textContent).toContain("a flow needs a name");
    expect(listedNames("flow-list")).toEqual(["Deploy"]);
  });

  test("moving a flow up or down reorders all flows, archived ones included", async () => {
    storedFlows = flows("a", "old", "b", "c");
    storedFlows[1].archived = true;
    await startApp();
    await openFlows();
    await click("c", "up");
    expect(callsOf("flows_reorder").at(-1)!.args.ids).toEqual([1, 2, 4, 3]);
    expect(listedNames("flow-list")).toEqual(["a", "c", "b"]);
    expect(boxRows()).toEqual(["a", "c", "b"]);
    await click("a", "down");
    expect(listedNames("flow-list")).toEqual(["c", "a", "b"]);
    // The ends cannot move further.
    expect(rowOf("c").querySelector<HTMLButtonElement>("button.up")!.disabled).toBe(true);
    expect(rowOf("b").querySelector<HTMLButtonElement>("button.down")!.disabled).toBe(true);
  });

  test("archiving hides a flow from the toolbox until it is unarchived", async () => {
    storedFlows = flows("Deploy", "Review");
    await startApp();
    await openFlows();
    await click("Deploy", "archive");
    expect(callsOf("flow_set_archived").map((c) => c.args)).toEqual([{ id: 1, archived: true }]);
    expect(boxRows()).toEqual(["Review"]);
    expect(listedNames("archived-list")).toEqual(["Deploy"]);
    await click("Deploy", "unarchive");
    expect(boxRows()).toEqual(["Deploy", "Review"]);
    expect(listedNames("archived-list")).toEqual([]);
  });

  test("deleting asks first", async () => {
    storedFlows = flows("Deploy", "Review", "Old");
    storedFlows[2].archived = true;
    await startApp();
    await openFlows();
    await click("Deploy", "delete");
    expect(picker.asked).toEqual([expect.stringContaining("Deploy")]);
    expect(callsOf("flow_delete")).toHaveLength(0);
    picker.answer = true;
    await click("Deploy", "delete");
    expect(callsOf("flow_delete").map((c) => c.args)).toEqual([{ id: 1 }]);
    expect(listedNames("flow-list")).toEqual(["Review"]);
    expect(boxRows()).toEqual(["Review"]);
    await click("Old", "delete");
    expect(listedNames("archived-list")).toEqual([]);
  });

  test("a flow that cannot be deleted stays, and the page says why", async () => {
    storedFlows = flows("Deploy");
    busyFlows.add(1);
    picker.answer = true;
    await startApp();
    await openFlows();
    await click("Deploy", "delete");
    expect(pageError().textContent).toContain("running");
    expect(listedNames("flow-list")).toEqual(["Deploy"]);
  });

  test("while the flows cannot be read, nothing on the page can be changed", async () => {
    storedFlows = flows("Deploy", "Old");
    storedFlows[1].archived = true;
    await startApp();
    await settle();
    failing.add("flows_list");
    await openFlows();
    expect(pageError().textContent).toContain("Flows could not be read");
    expect(pageError().textContent).toContain("reopen");
    const controls = [...page().querySelectorAll<HTMLInputElement | HTMLButtonElement>("input, button")];
    expect(controls.length).toBeGreaterThan(3);
    for (const c of controls) expect(c.matches(":disabled"), c.outerHTML).toBe(true);
    // Reopening once they can be read again enables the page.
    failing.delete("flows_list");
    tabs().at(-1)!.querySelector<HTMLButtonElement>(".x")!.click();
    await openFlows();
    expect(pageError().hidden).toBe(true);
    for (const f of page().querySelectorAll("fieldset")) expect(f.disabled).toBe(false);
  });

  test("a change whose reload fails says the list may be out of date", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFlows();
    failing.add("flows_list");
    await create("Review");
    expect(storedFlows.map((f) => f.name)).toEqual(["Deploy", "Review"]);
    expect(pageError().textContent).toContain("Flows could not be read");
  });

  test("changes run one at a time, in order", async () => {
    storedFlows = flows("a", "b", "c");
    await startApp();
    await openFlows();
    // Two quick clicks: the second works on the order the first left.
    rowOf("c").querySelector<HTMLButtonElement>("button.up")!.click();
    rowOf("c").querySelector<HTMLButtonElement>("button.up")!.click();
    await settle();
    await settle();
    expect(pageError().hidden).toBe(true);
    expect(listedNames("flow-list")).toEqual(["c", "a", "b"]);
  });
});

describe("flow editor", () => {
  const SAVE_MS = 400;
  const flows = (...names: string[]) => names.map((name, i) => ({ id: i + 1, name, archived: false }));
  const step = (id: string, kind: string, title: string, outputs: string[], text = ""): Step => ({ id, kind, title, text, outputs, x: 0, y: 0 });
  /** Start → P, whose pass goes to E and fail back to P. */
  const runnable = (): Graph => ({
    nodes: [START, step("p", "prompt", "P", ["pass", "fail"], "do it"), step("e", "end", "E", [])],
    edges: [{ from: "start", output: "next", to: "p" }, { from: "p", output: "pass", to: "e" }, { from: "p", output: "fail", to: "p" }],
  });
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const editor = () => document.querySelector<HTMLElement>(".page.flow-editor.active")!;
  const editors = () => [...document.querySelectorAll<HTMLElement>(".page.flow-editor")];
  const nodes = () => [...editor().querySelectorAll<HTMLElement>(".drawflow-node")];
  const titles = () => nodes().map((n) => n.querySelector(".title")!.textContent);
  const node = (title: string) => nodes().find((n) => n.querySelector(".title")!.textContent === title)!;
  const connections = () => editor().querySelectorAll("svg.connection").length;
  const panel = () => editor().querySelector<HTMLElement>(".panel")!;
  const field = (name: string) => panel().querySelector<HTMLInputElement | HTMLTextAreaElement>(`[name=${name}]`)!;
  const tool = (cls: string) => editor().querySelector<HTMLButtonElement>(`button.${cls}`)!;
  const saved = () => callsOf("flow_save_graph").at(-1)?.args.graph as Graph | undefined;
  const mouse = (el: Element, type: string, x = 0, y = 0) =>
    el.dispatchEvent(new MouseEvent(type, { bubbles: true, clientX: x, clientY: y, button: 0 }));
  async function openFromBox(name: string) {
    [...document.querySelectorAll<HTMLElement>("#flows-box .flows li")].find((li) => li.textContent === name)!.click();
    await settle();
  }
  /** Drags from `from`'s output number `output` (1-based) to `to`'s input, as a user connects them. */
  async function connect(from: string, output: number, to: string) {
    const target = node(to).querySelector(".input_1")!;
    mouse(node(from).querySelector(`.output_${output}`)!, "mousedown");
    mouse(target, "mousemove", 50, 0);
    mouse(target, "mouseup", 50, 0);
    await settle();
  }
  async function select(title: string) {
    const content = node(title).querySelector(".drawflow_content_node")!;
    mouse(content, "mousedown");
    mouse(content, "mouseup");
    await settle();
  }
  async function edit(name: string, value: string) {
    field(name).value = value;
    field(name).dispatchEvent(new Event("input", { bubbles: true }));
    field(name).dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
  }
  async function afterSave() {
    await vi.advanceTimersByTimeAsync(SAVE_MS);
    await settle();
  }

  test("clicking a flow in the toolbox opens its editor, one tab per flow", async () => {
    storedFlows = flows("Deploy", "Review");
    await startApp();
    await openClaudeTabs(1);
    await openFromBox("Deploy");
    expect(tabNames()).toEqual(["work", "Deploy"]);
    expect(callsOf("flow_load").map((c) => c.args)).toEqual([{ id: 1 }]);
    await openFromBox("Deploy");
    await openFromBox("Review");
    expect(tabNames()).toEqual(["work", "Deploy", "Review"]);
    expect(editors()).toHaveLength(2);
    expect(editor().dataset.flow).toBe("2");
    await win.closeRequested!();
    expect(lastSaved()!.tabs.map((t) => t.name)).toEqual(["Claude 1"]);
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    await settle();
    expect(tabNames()).toEqual(["work", "Review"]);
    expect(editors()).toHaveLength(1);
  });

  test("the Flows page opens the editor too", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    document.querySelector<HTMLButtonElement>("#flows-box .configure")!.click();
    await settle();
    document.querySelector<HTMLButtonElement>("#flows-settings .flow-list li button.edit")!.click();
    await settle();
    expect(tabNames()).toEqual(["Flows", "Deploy"]);
    expect(editor().dataset.flow).toBe("1");
  });

  test("the canvas shows the steps, their outputs and their connections", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    expect(titles()).toEqual(["Start", "P", "E"]);
    expect(nodes().map((n) => ["start", "prompt", "end"].find((k) => n.classList.contains(k)))).toEqual(["start", "prompt", "end"]);
    expect([...node("P").querySelectorAll<HTMLElement>(".output")].map((o) => o.dataset.label)).toEqual(["pass", "fail"]);
    expect(node("Start").querySelector(".input")).toBeNull();
    expect(node("E").querySelector(".output")).toBeNull();
    expect(connections()).toBe(3);
    expect(callsOf("flow_save_graph")).toHaveLength(0);
  });

  test("an added step is saved after a short pause", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-prompt").click();
    tool("add-pause").click();
    tool("add-end").click();
    await settle();
    expect(titles()).toEqual(["Start", "Prompt", "Pause", "End"]);
    expect(callsOf("flow_save_graph")).toHaveLength(0);
    await afterSave();
    expect(callsOf("flow_save_graph")).toHaveLength(1);
    const graph = saved()!;
    expect(graph.nodes.map((n) => [n.kind, n.outputs])).toEqual([
      ["start", ["next"]],
      ["prompt", ["done"]],
      ["manual", ["done"]],
      ["end", []],
    ]);
    expect(new Set(graph.nodes.map((n) => n.id)).size).toBe(4);
    expect(editor().querySelector(".status")!.textContent).toBe("Saved");
  });

  test("connecting an output saves it, replacing where that output went before", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = { nodes: [START, step("a", "prompt", "A", ["done"], "x"), step("b", "end", "B", [])], edges: [] };
    await startApp();
    await openFromBox("Deploy");
    await connect("Start", 1, "A");
    await connect("A", 1, "B");
    await connect("Start", 1, "B");
    await afterSave();
    expect(saved()!.edges).toEqual([
      { from: "a", output: "done", to: "b" },
      { from: "start", output: "next", to: "b" },
    ]);
    expect(connections()).toBe(2);
  });

  test("a selected step's fields are shown and saved, and its outputs become ports", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    expect(panel().hidden).toBe(true);
    await select("P");
    expect(panel().hidden).toBe(false);
    expect(field("title").value).toBe("P");
    expect(field("text").value).toBe("do it");
    expect(field("outputs").value).toBe("pass, fail");
    await edit("title", "Fix tests");
    await edit("text", "Run the tests and fix them.");
    await edit("outputs", "Pass, retry");
    await afterSave();
    const p = saved()!.nodes.find((n) => n.id === "p")!;
    expect([p.title, p.text, p.outputs]).toEqual(["Fix tests", "Run the tests and fix them.", ["pass", "retry"]]);
    // The fail output is gone, and with it its connection.
    expect(saved()!.edges).toEqual([{ from: "start", output: "next", to: "p" }, { from: "p", output: "pass", to: "e" }]);
    expect([...node("Fix tests").querySelectorAll<HTMLElement>(".output")].map((o) => o.dataset.label)).toEqual(["pass", "retry"]);
    expect(connections()).toBe(2);
  });

  test("outputs that are not short words are refused in place", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    await edit("outputs", "ok, needs work");
    expect(panel().querySelector(".invalid")!.textContent).toContain("needs work");
    await edit("outputs", " ");
    expect(panel().querySelector(".invalid")!.textContent).toContain("at least one");
    await edit("outputs", "a, b, c, d, e, f, g, h, i");
    expect(panel().querySelector(".invalid")!.textContent).toContain("at most 8");
    await afterSave();
    expect(callsOf("flow_save_graph")).toHaveLength(0);
    expect(field("title").maxLength).toBe(200);
    expect(field("text").maxLength).toBe(20000);
  });

  test("a step can be limited to a number of runs, for loops", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    expect(field("maxRuns").value).toBe("");
    await edit("maxRuns", "3");
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "p")).toMatchObject({ maxRuns: 3 });
    // The canvas shows the limit, so a loop's cap can be seen without selecting the step.
    expect(node("P").querySelector(".limit")!.textContent).toBe("≤3");
    expect((field("maxRuns") as HTMLInputElement).max).toBe("100");
    await edit("maxRuns", "0");
    expect(panel().querySelector(".runs-invalid")!.textContent).toContain("1 to 100");
    await edit("maxRuns", "");
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "p")).not.toHaveProperty("maxRuns");
    expect(node("P").querySelector(".limit")).toBeNull();
    await select("Start");
    expect(field("maxRuns").closest("label")!.hidden).toBe(true);
    // Leaving a manual step is always your pick, which no limit holds back.
    tool("add-pause").click();
    await settle();
    await select("Pause");
    expect(field("maxRuns").closest("label")!.hidden).toBe(true);
  });

  test("Start can begin each run with /clear, shown on the canvas and in the toolbox", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    const mark = () => document.querySelector("#flows-box .flows li .lucide")!;
    expect(mark().classList.contains("clears")).toBe(false);
    await select("P");
    expect(field("clear").closest("label")!.hidden).toBe(true);
    await select("Start");
    expect(field("clear").closest("label")!.hidden).toBe(false);
    expect((field("clear") as HTMLInputElement).checked).toBe(false);
    const box = field("clear") as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event("change", { bubbles: true }));
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "start")).toMatchObject({ clear: true });
    expect(node("Start").querySelector(".clears")!.textContent).toBe("/clear");
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    await settle();
    expect(mark().classList.contains("clears")).toBe(true);
    box.checked = false;
    box.dispatchEvent(new Event("change", { bubbles: true }));
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "start")).not.toHaveProperty("clear");
    expect(node("Start").querySelector(".clears")).toBeNull();
  });

  test("Start has an optional base info text, saved with the flow", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("Start");
    const text = field("text") as HTMLTextAreaElement;
    expect(text.closest("label")!.hidden).toBe(false);
    expect(panel().querySelector(".text-label")!.textContent).toBe("Base info");
    expect(field("outputs").closest("label")!.hidden).toBe(true);
    text.value = "The app is in src/.";
    text.dispatchEvent(new Event("input", { bubbles: true }));
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "start")).toMatchObject({ text: "The app is in src/." });
    expect((field("text") as HTMLTextAreaElement).maxLength).toBe(4000);
    // The canvas shows that Start sends something, so reading the flow does not miss it.
    expect(node("Start").querySelector(".info")!.textContent).toBe("info");
    await select("P");
    expect(panel().querySelector(".text-label")!.textContent).toBe("Prompt");
    expect((field("text") as HTMLTextAreaElement).maxLength).toBe(20000);
    await select("Start");
    text.value = "  ";
    text.dispatchEvent(new Event("input", { bubbles: true }));
    await afterSave();
    expect(node("Start").querySelector(".info")).toBeNull();
  });

  test("▶ saves a change to the flow first, so a just-ticked /clear counts", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openClaudeTabs(1);
    await openFromBox("Deploy");
    await select("Start");
    const box = field("clear") as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    // Not saved yet when ▶ is pressed.
    expect(callsOf("flow_save_graph")).toHaveLength(0);
    document.querySelector<HTMLButtonElement>("#flows-box .flows li button.run")!.click();
    await settle();
    expect(callsOf("flow_save_graph")).toHaveLength(1);
    expect(callsOf("flow_start")).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(200);
    const typed = callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
    expect(typed).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
  });

  test("a saved run limit is shown, and kept when another step is selected before it is committed", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    storedGraphs[1].nodes[1] = { ...storedGraphs[1].nodes[1], maxRuns: 5 } as Step;
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    expect(field("maxRuns").value).toBe("5");
    field("maxRuns").value = "2";
    await select("E");
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "p")).toMatchObject({ maxRuns: 2 });
  });

  test("a run limit edit does not hide what is wrong with the outputs", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    await edit("outputs", "ok, needs work");
    await edit("maxRuns", "3");
    const shown = [...panel().querySelectorAll<HTMLElement>(".invalid")].filter((e) => !e.hidden).map((e) => e.textContent);
    expect(shown).toEqual([expect.stringContaining("needs work")]);
    await edit("maxRuns", "101");
    const both = [...panel().querySelectorAll<HTMLElement>(".invalid")].filter((e) => !e.hidden);
    expect(both).toHaveLength(2);
  });

  test("an outputs edit is kept when another step is selected before it is committed", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    field("outputs").value = "pass, fail, skip";
    await select("E");
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "p")!.outputs).toEqual(["pass", "fail", "skip"]);
  });

  test("a new step never takes the id of a deleted one", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-pause").click();
    await afterSave();
    const first = saved()!.nodes[1].id;
    await select("Pause");
    panel().querySelector<HTMLButtonElement>("button.delete-step")!.click();
    tool("add-pause").click();
    await afterSave();
    expect(saved()!.nodes[1].id).not.toBe(first);
  });

  test("a manual step has no outputs field, as it goes on one way", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-pause").click();
    await settle();
    await select("Pause");
    expect(field("outputs").closest("label")!.hidden).toBe(true);
  });

  test("a manual prompt has a draft, one output and no run limit", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-manual-prompt").click();
    await settle();
    await select("Manual prompt");
    expect(field("text").closest("label")!.hidden).toBe(false);
    expect(panel().querySelector(".text-label")!.textContent).toContain("Draft");
    expect(field("outputs").closest("label")!.hidden).toBe(true);
    expect(field("maxRuns").closest("label")!.hidden).toBe(true);
    await afterSave();
    expect(saved()!.nodes.map((n) => [n.kind, n.outputs])).toEqual([
      ["start", ["next"]],
      ["manual_prompt", ["sent"]],
    ]);
  });

  test("a manual prompt from a flow file with several outputs says it has one", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = { nodes: [START, step("m", "manual_prompt", "Ask", ["a", "b"])], edges: [] };
    await startApp();
    await openFromBox("Deploy");
    await select("Ask");
    await edit("outputs", "a, c");
    expect(panel().querySelector<HTMLElement>(".invalid")!.textContent).toContain("A manual prompt has one output");
  });

  test("a manual step from an older flow with several outputs shows them, to cut down to one", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = {
      nodes: [START, step("m", "manual", "Review", ["approve", "reject"]), step("p", "prompt", "Fix", ["done"], "fix it")],
      edges: [{ from: "m", output: "reject", to: "p" }],
    };
    await startApp();
    await openFromBox("Deploy");
    await select("Review");
    expect(field("outputs").closest("label")!.hidden).toBe(false);
    await edit("outputs", "approve, again");
    expect(panel().querySelector<HTMLElement>(".invalid")!.textContent).toContain("A pause has one output");
    await edit("outputs", "approve");
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "m")!.outputs).toEqual(["approve"]);
    // Its connection from "reject" went with it, and the panel says so.
    expect(saved()!.edges).toEqual([]);
    expect(panel().querySelector<HTMLElement>(".removed")!.textContent).toBe('Removed the connection from "reject".');
    await select("Start");
    expect(panel().querySelector<HTMLElement>(".removed")!.hidden).toBe(true);
    await select("Review");
    expect(field("outputs").closest("label")!.hidden).toBe(true);
  });

  test("Start and End have no fields for outputs, End none for a prompt, and Start cannot be deleted", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("Start");
    expect(field("outputs").closest("label")!.hidden).toBe(true);
    expect(panel().querySelector<HTMLButtonElement>("button.delete-step")!.hidden).toBe(true);
    editor().querySelector(".canvas")!.firstElementChild!.dispatchEvent(new KeyboardEvent("keydown", { key: "Delete", bubbles: true }));
    await settle();
    expect(titles()).toEqual(["Start", "P", "E"]);
    expect(connections()).toBe(3);
    // Right-clicking Start offers no delete either.
    mouse(node("Start"), "contextmenu");
    const cross = node("Start").querySelector(".drawflow-delete");
    if (cross) mouse(cross, "mousedown");
    await settle();
    expect(titles()).toEqual(["Start", "P", "E"]);
    expect(connections()).toBe(3);
    // Other steps are deleted by the same key.
    await select("E");
    expect(field("text").closest("label")!.hidden).toBe(true);
    expect(panel().querySelector<HTMLButtonElement>("button.delete-step")!.hidden).toBe(false);
    editor().querySelector(".canvas")!.firstElementChild!.dispatchEvent(new KeyboardEvent("keydown", { key: "Delete", bubbles: true }));
    await afterSave();
    expect(titles()).toEqual(["Start", "P"]);
    expect(saved()!.edges).toEqual([{ from: "start", output: "next", to: "p" }, { from: "p", output: "fail", to: "p" }]);
  });

  test("deleting a step takes its connections with it", async () => {
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    await select("P");
    panel().querySelector<HTMLButtonElement>("button.delete-step")!.click();
    await afterSave();
    expect(saved()!.nodes.map((n) => n.id)).toEqual(["start", "e"]);
    expect(saved()!.edges).toEqual([]);
    expect(panel().hidden).toBe(true);
  });

  test("a moved step is saved where it was dropped", async () => {
    // jsdom has no layout; Drawflow positions steps by their inline left and top.
    for (const [prop, style] of [["offsetLeft", "left"], ["offsetTop", "top"]] as const) {
      vi.spyOn(HTMLElement.prototype, prop, "get").mockImplementation(function (this: HTMLElement) {
        return parseFloat(this.style[style]) || 0;
      });
    }
    for (const prop of ["clientWidth", "clientHeight"] as const) vi.spyOn(HTMLElement.prototype, prop, "get").mockReturnValue(1000);
    storedFlows = flows("Deploy");
    storedGraphs[1] = runnable();
    await startApp();
    await openFromBox("Deploy");
    const content = node("E").querySelector(".drawflow_content_node")!;
    mouse(content, "mousedown", 0, 0);
    mouse(content, "mousemove", 30, 40);
    mouse(content, "mouseup", 30, 40);
    await afterSave();
    expect(saved()!.nodes.find((n) => n.id === "e")).toMatchObject({ x: 30, y: 40 });
  });

  test("what keeps the flow from running is listed until it is ready", async () => {
    storedFlows = flows("Deploy");
    flowProblems = ["Start is not connected to a step."];
    await startApp();
    await openFromBox("Deploy");
    const problems = () => [...editor().querySelectorAll(".problems li")].map((li) => li.textContent);
    expect(problems()).toEqual(["Start is not connected to a step."]);
    flowProblems = [];
    tool("add-pause").click();
    await afterSave();
    expect(problems()).toEqual([]);
    expect(editor().querySelector(".problems")!.textContent).toContain("Ready to run");
  });

  test("a failed save says why, and the next change tries again", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    failing.add("flow_save_graph");
    tool("add-pause").click();
    await afterSave();
    expect(editor().querySelector<HTMLElement>(".error")!.hidden).toBe(false);
    expect(editor().querySelector(".error")!.textContent).toContain("flow_save_graph failed");
    failing.delete("flow_save_graph");
    tool("add-pause").click();
    await afterSave();
    expect(editor().querySelector<HTMLElement>(".error")!.hidden).toBe(true);
    expect(storedGraphs[1].nodes).toHaveLength(3);
  });

  test("closing the tab saves a change still waiting", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-pause").click();
    await settle();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await settle();
    expect(storedGraphs[1].nodes.map((n) => n.kind)).toEqual(["start", "manual"]);
    expect(tabs()).toHaveLength(0);
  });

  test("a tab whose changes cannot be saved stays open and says so", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    failing.add("flow_save_graph");
    tool("add-pause").click();
    await settle();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await settle();
    expect(tabNames()).toEqual(["Deploy"]);
    expect(editor().querySelector(".error")!.textContent).toContain("Not saved");
    // Once saving works again, closing saves and closes.
    failing.delete("flow_save_graph");
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await settle();
    expect(tabs()).toHaveLength(0);
    expect(storedGraphs[1].nodes).toHaveLength(2);
  });

  test("quitting saves the open editors' waiting changes", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    tool("add-pause").click();
    await settle();
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
    expect(storedGraphs[1].nodes).toHaveLength(2);
  });

  test("quitting with unsavable editor changes warns once", async () => {
    storedFlows = flows("Deploy");
    await startApp();
    await openFromBox("Deploy");
    failing.add("flow_save_graph");
    tool("add-pause").click();
    await settle();
    await win.closeRequested!();
    expect(win.prevented).toBe(true);
    expect(noticeText()).toContain("not saved");
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
  });

  test("a flow that cannot be loaded says so and cannot be edited", async () => {
    storedFlows = flows("Deploy");
    failing.add("flow_load");
    await startApp();
    await openFromBox("Deploy");
    expect(editor().querySelector(".error")!.textContent).toContain("could not be loaded");
    expect(tool("add-prompt").matches(":disabled")).toBe(true);
  });

  test("renaming a flow retitles its tab, and deleting it closes the tab", async () => {
    storedFlows = flows("Deploy", "Review");
    picker.answer = true;
    await startApp();
    await openFromBox("Deploy");
    document.querySelector<HTMLButtonElement>("#flows-box .configure")!.click();
    await settle();
    const rows = () => [...document.querySelectorAll<HTMLElement>("#flows-settings .flow-list li")];
    const name = rows()[0].querySelector<HTMLInputElement>("input.name")!;
    name.value = "Ship";
    name.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(tabNames()).toEqual(["Ship", "Flows"]);
    // A change still waiting is dropped with the flow, not saved into nothing.
    document.querySelector<HTMLElement>(".page.flow-editor button.add-pause")!.click();
    rows()[0].querySelector<HTMLButtonElement>("button.delete")!.click();
    await settle();
    await vi.advanceTimersByTimeAsync(SAVE_MS);
    expect(tabNames()).toEqual(["Flows"]);
    expect(editors()).toHaveLength(0);
    expect(callsOf("flow_save_graph")).toHaveLength(0);
  });
});

describe("flow runs", () => {
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
  const box = () => document.querySelector<HTMLElement>("#flows-box")!;
  const flowRow = (name: string) => [...box().querySelectorAll<HTMLElement>(".flows > li")].find((li) => li.querySelector(".name")!.textContent === name)!;
  const running = () => [...box().querySelectorAll<HTMLElement>(".flows > li.running")].map((li) => li.querySelector(".name")!.textContent);
  // One editor per test: found whether or not it is the page shown.
  const editor = () => document.querySelector<HTMLElement>(".page.flow-editor")!;
  const runBar = () => editor().querySelector<HTMLElement>(".run-bar")!;
  const outputs = () => [...runBar().querySelectorAll<HTMLButtonElement>("button.output")];
  const current = () => [...editor().querySelectorAll<HTMLElement>(".drawflow-node.current")].map((n) => n.querySelector(".title")!.textContent);
  const step = (id: string, kind: Step["kind"], title: string, outputs: string[]): Step => ({ id, kind, title, text: "x", outputs, x: 0, y: 0 });
  const run = (over: Partial<Run> = {}): Run => ({
    id: 5, flowId: 1, flowName: "Deploy", tabId: "", stepId: "p", step: "Fix the tests",
    status: "running", note: null, outputs: ["pass", "fail"], steps: 1, done: 0, total: 2, resumable: false, prefill: false, stepKind: "prompt", briefing: false, ...over,
  });
  async function poll() {
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    await settle();
  }
  async function play(name: string) {
    flowRow(name).querySelector<HTMLButtonElement>("button.run")!.click();
    await settle();
    // The Enter that submits a typed prompt follows the paste.
    await vi.advanceTimersByTimeAsync(200);
  }
  async function open(name: string) {
    flowRow(name).querySelector<HTMLElement>(".name")!.click();
    await settle();
  }

  beforeEach(() => {
    storedFlows = [{ id: 1, name: "Deploy", archived: false }, { id: 2, name: "Review", archived: false }];
    storedGraphs[1] = {
      nodes: [START, step("p", "prompt", "Fix the tests", ["pass", "fail"]), step("m", "manual", "Check the form", ["ok"])],
      edges: [{ from: "start", output: "next", to: "p" }, { from: "p", output: "pass", to: "m" }],
    };
  });

  test("▶ starts a flow in the active Claude tab and types what the run sends first (its briefing)", async () => {
    runPrompt = "Fix the tests.\nEnd with FLOW: pass";
    await startApp();
    await openClaudeTabs(2);
    tabs()[0].click();
    await play("Deploy");
    expect(callsOf("flow_start").map((c) => c.args)).toEqual([{ flowId: 1, tabId: spawns()[0].tabId }]);
    expect(typed()).toEqual(["\x1b[200~Fix the tests.\nEnd with FLOW: pass\x1b[201~", "\r"]);
    expect(callsOf("pty_write")[0].args.id).toBe(1);
  });

  test("▶ on a flow added from a tab asks first, and runs it only once it is confirmed", async () => {
    runPrompt = "Fix the tests.";
    storedFlows[0].unconfirmed = true;
    await startApp();
    await openClaudeTabs(1);
    await play("Deploy");
    expect(picker.asked).toEqual([expect.stringContaining("added from a tab")]);
    expect(picker.asked[0]).toContain("Start's base info");
    expect(callsOf("flow_confirm")).toHaveLength(0);
    expect(callsOf("flow_start")).toHaveLength(0);
    picker.answer = true;
    await play("Deploy");
    expect(callsOf("flow_confirm").map((c) => c.args)).toEqual([{ id: 1 }]);
    expect(callsOf("flow_start")).toHaveLength(1);
    expect(typed()).toEqual(["\x1b[200~Fix the tests.\x1b[201~", "\r"]);
    // Confirmed once: the next ▶ does not ask again.
    expect(storedFlows[0].unconfirmed).toBeUndefined();
  });

  /** Presses ▶ on a flow, then lets the paste's Enter go. */
  async function playAndEnter(name: string) {
    flowRow(name).querySelector<HTMLButtonElement>("button.run")!.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    await settle();
  }
  async function pollClear() {
    await vi.advanceTimersByTimeAsync(CLEAR_POLL_MS);
    await settle();
  }

  test("a flow whose Start clears starts its run, types /clear, and sends the briefing once Claude has cleared", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    clearStates = [{ state: "pending" }, { state: "ready", prompt: "Fix the tests." }];
    await startApp();
    await openClaudeTabs(1);
    await playAndEnter("Deploy");
    // The run holds the tab before anything is typed, so a refused start clears nothing.
    expect(callsOf("flow_start")).toHaveLength(1);
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
    await pollClear();
    expect(callsOf("flow_cleared").map((c) => c.args)).toEqual([{ runId: 5 }]);
    expect(typed()).toHaveLength(2);
    await pollClear();
    await vi.advanceTimersByTimeAsync(200);
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~", "\r", "\x1b[200~Fix the tests.\x1b[201~", "\r"]);
    // Done: no more asking.
    await pollClear();
    expect(callsOf("flow_cleared")).toHaveLength(2);
    expect(callsOf("flow_stop")).toHaveLength(0);
  });

  test("a clear that never comes stops the run and says so", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    await startApp();
    await openClaudeTabs(1);
    await playAndEnter("Deploy");
    await vi.advanceTimersByTimeAsync(CLEAR_TIMEOUT_MS);
    await settle();
    expect(callsOf("flow_stop").map((c) => c.args)).toEqual([{ runId: 5 }]);
    expect(box().querySelector(".error")!.textContent).toContain("did not clear");
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
  });

  test("a run stopped while it clears is left alone", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    clearStates = [{ state: "gone" }];
    await startApp();
    await openClaudeTabs(1);
    await playAndEnter("Deploy");
    await pollClear();
    await vi.advanceTimersByTimeAsync(CLEAR_TIMEOUT_MS);
    await settle();
    expect(callsOf("flow_cleared")).toHaveLength(1);
    expect(callsOf("flow_stop")).toHaveLength(0);
    expect(box().querySelector<HTMLElement>(".error")!.hidden).toBe(true);
  });

  test("a flow that clears does not start while Claude is working, as /clear would wait for it", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    await startApp();
    await openClaudeTabs(1);
    terms()[0].onTitle!("◐ Fixing");
    await playAndEnter("Deploy");
    expect(callsOf("flow_start")).toHaveLength(0);
    expect(typed()).toEqual([]);
    expect(box().querySelector(".error")!.textContent).toContain("working");
  });

  test("no flow starts while Claude is working, as the turn's end would be taken for the briefing's reply", async () => {
    runPrompt = "The briefing.";
    await startApp();
    await openClaudeTabs(1);
    terms()[0].onTitle!("◐ Fixing");
    await play("Deploy");
    expect(callsOf("flow_start")).toHaveLength(0);
    expect(typed()).toEqual([]);
    expect(box().querySelector(".error")!.textContent).toContain("working");
  });

  test("a flow that does not clear does not start while Claude waits on background tasks, as the turn they wake it for would be taken for the briefing's reply", async () => {
    runPrompt = "The briefing.";
    await startApp();
    await openClaudeTabs(1);
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    await play("Deploy");
    expect(callsOf("flow_start")).toHaveLength(0);
    expect(typed()).toEqual([]);
    expect(box().querySelector(".error")!.textContent).toContain("waiting on background tasks");
  });

  test("a flow that clears still starts while Claude only waits on background tasks, as Claude is idle", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    await startApp();
    await openClaudeTabs(1);
    backgroundTabs = { [spawns()[0].tabId]: Date.now() };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(tabs()[0].classList.contains("busy")).toBe(true);
    await playAndEnter("Deploy");
    expect(callsOf("flow_stop")).toHaveLength(0);
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
  });

  test("pressing ▶ again while a flow starts in the tab does not start it twice", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    await startApp();
    await openClaudeTabs(1);
    const play = flowRow("Deploy").querySelector<HTMLButtonElement>("button.run")!;
    play.click();
    play.click();
    flowRow("Review").querySelector<HTMLButtonElement>("button.run")!.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_start")).toHaveLength(1);
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~", "\r"]);
  });

  test("a flow that is not ready is refused without clearing", async () => {
    storedGraphs[1].nodes[0] = { ...START, clear: true };
    flowProblems = ["Start is not connected to a step."];
    runRefusal = "the flow is not ready to run: Start is not connected to a step.";
    await startApp();
    await openClaudeTabs(1);
    await play("Deploy");
    expect(typed()).toEqual([]);
    expect(box().querySelector(".error")!.textContent).toContain("not ready to run");
  });

  test("a flow that starts at a manual step types nothing", async () => {
    await startApp();
    await openClaudeTabs(1);
    await play("Deploy");
    expect(callsOf("flow_start")).toHaveLength(1);
    expect(typed()).toEqual([]);
  });

  test("a run at a manual prompt fills Claude's input once, without sending it", async () => {
    prefills = [{ text: "The app is in src/.\n\nFix the bug in ", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    activeRuns = [run({ tabId, status: "manual", stepId: "mp", step: "Ask", outputs: ["sent"], prefill: true })];
    await poll();
    await poll();
    expect(callsOf("flow_prefill_text")[0].args).toEqual({ runId: 5, stepId: "mp" });
    // Only once it is in Claude's input does the backend stop owing it.
    expect(callsOf("flow_prefilled").map((c) => c.args)).toEqual([{ runId: 5, stepId: "mp", token: 7 }]);
    expect(typed()).toEqual(["\x1b[200~The app is in src/.\n\nFix the bug in \x1b[201~"]);
    activeRuns = [run({ tabId, status: "manual", stepId: "mp", step: "Ask", outputs: ["sent"] })];
    await poll();
    expect(typed()).toHaveLength(1);
  });

  test("a manual prompt that cannot be filled is tried again, saying so once", async () => {
    prefillError = "database is locked";
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "manual", stepId: "mp", step: "Ask", outputs: ["sent"], prefill: true })];
    await poll();
    expect(box().querySelector(".error")!.textContent).toContain("database is locked");
    box().querySelector<HTMLElement>(".error")!.textContent = "";
    await poll();
    expect(box().querySelector(".error")!.textContent).toBe("");
    prefillError = null;
    await poll();
    expect(typed()).toEqual(["\x1b[200~Draft\x1b[201~"]);
  });

  /** A run waiting at the manual prompt "Ask" in the first Claude tab. */
  const atManualPrompt = (over: Partial<Run> = {}) =>
    run({ tabId: spawns()[0].tabId, status: "manual", stepId: "mp", step: "Ask", outputs: ["sent"], stepKind: "manual_prompt", ...over });

  test("a manual prompt does not wipe what the person is typing, and says how to get its draft", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    terms()[0].onData!("half a thought");
    activeRuns = [atManualPrompt({ prefill: true })];
    await poll();
    await poll();
    expect(typed()).toEqual(["half a thought"]);
    expect(callsOf("flow_prefill_text")).toHaveLength(0);
    expect(box().querySelector(".error")!.textContent).toContain("clear it");
    // Once the input is cleared (Ctrl+C), the draft goes in.
    terms()[0].onData!("\x03");
    await poll();
    expect(typed()).toEqual(["half a thought", "\x03", "\x1b[200~Draft\x1b[201~"]);
  });

  test("skipping a manual prompt takes its untouched draft back out of Claude's input", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [atManualPrompt({ prefill: true })];
    await poll();
    activeRuns = [atManualPrompt()];
    await poll();
    const go = flowRow("Deploy").querySelector<HTMLButtonElement>("button.go-on")!;
    expect(go.title).toBe('Skip "Ask" without sending a prompt');
    expect(flowRow("Deploy").title).toContain("waiting for your prompt");
    go.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    // The next step is a pause: nothing is typed but the clear.
    expect(typed()).toEqual(["\x1b[200~Draft\x1b[201~", "\x03"]);
  });

  test("a draft the person has added to stays when the run is stopped", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [atManualPrompt({ prefill: true })];
    await poll();
    terms()[0].onData!(" more");
    activeRuns = [atManualPrompt()];
    await poll();
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.stop-run")!.click();
    await settle();
    expect(typed()).toEqual(["\x1b[200~Draft\x1b[201~", " more"]);
  });

  test("stopping a run takes its untouched manual prompt draft out of Claude's input", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [atManualPrompt({ prefill: true })];
    await poll();
    activeRuns = [atManualPrompt()];
    await poll();
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.stop-run")!.click();
    await settle();
    expect(typed()).toEqual(["\x1b[200~Draft\x1b[201~", "\x03"]);
  });

  test("a paste that could not be recorded says it was pasted", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    prefilledError = "database is locked";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [atManualPrompt({ prefill: true })];
    await poll();
    expect(box().querySelector(".error")!.textContent).toContain("Pasted");
  });

  test("stopping a run types nothing into Claude, whose next prompt is told the flow is finished", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.stop-run")!.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(typed()).toEqual([]);
  });

  test("a run says it is briefing Claude before its first step", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, stepId: "start", step: "Start", outputs: [], stepKind: "start", briefing: true })];
    await poll();
    expect(flowRow("Deploy").title).toContain("briefing claude at Start");
    // Clearing first, the run is not briefing yet.
    activeRuns = [run({ tabId: spawns()[0].tabId, stepId: "start", step: "Start", outputs: [], stepKind: "start", note: "Clearing the conversation before the first step." })];
    await poll();
    expect(flowRow("Deploy").title).toContain("running at Start");
  });

  test("the run bar offers a manual prompt's output as a skip", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [atManualPrompt()];
    await poll();
    await open("Deploy");
    expect(runBar().querySelector(".where")!.textContent).toBe("Waiting for your prompt: Ask (no longer in this flow)");
    expect(outputs()[0].title).toBe('Skip without sending your prompt: go on as "sent"');
  });

  test("a manual prompt waits to fill the input while Claude is working", async () => {
    prefills = [{ text: "Draft", token: 7 }];
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    terms()[0].onTitle!("◐ Claude Code");
    activeRuns = [run({ tabId, status: "manual", stepId: "mp", step: "Ask", outputs: ["sent"], prefill: true })];
    await poll();
    expect(callsOf("flow_prefill_text")).toHaveLength(0);
    terms()[0].onTitle!("✳ Claude Code");
    await poll();
    expect(typed()).toEqual(["\x1b[200~Draft\x1b[201~"]);
  });

  test("without Claude in the active tab, ▶ says where flows run", async () => {
    await startApp();
    await play("Deploy");
    await clickTerminal();
    await play("Deploy");
    expect(callsOf("flow_start")).toHaveLength(0);
    expect(box().querySelector(".error")!.textContent).toContain("Claude tab");
  });

  test("a start that is refused says why", async () => {
    runRefusal = "the flow is not ready to run: Start is not connected to a step.";
    await startApp();
    await openClaudeTabs(1);
    await play("Deploy");
    expect(box().querySelector(".error")!.textContent).toContain("not ready to run");
    expect(typed()).toEqual([]);
  });

  test("only a flow running in the active tab is marked, with ■ in place of ▶", async () => {
    await startApp();
    await openClaudeTabs(2);
    const [a, b] = spawns().map((c) => c.tabId);
    activeRuns = [run({ tabId: a }), run({ id: 6, flowId: 2, tabId: b, status: "manual" })];
    await poll();
    // The second tab is active.
    expect(running()).toEqual(["Review"]);
    expect(flowRow("Review").querySelector("button.run")).toBeNull();
    expect(flowRow("Review").querySelector("button.stop-run")).not.toBeNull();
    expect(flowRow("Deploy").querySelector("button.run")).not.toBeNull();
    expect(box().querySelector(".runs")).toBeNull();
    tabs()[0].click();
    expect(running()).toEqual(["Deploy"]);
    expect(tabs().map((t) => t.classList.contains("needs-you"))).toEqual([false, true]);
    activeRuns = [];
    await poll();
    expect(running()).toEqual([]);
    expect(tabs().map((t) => t.classList.contains("needs-you"))).toEqual([false, false]);
  });

  test("▶ on a run that needs attention goes on to the next step", async () => {
    runPrompt = "Deploy it.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "attention", note: "Claude asked for a person.", outputs: ["done"], resumable: true })];
    await poll();
    const row = flowRow("Deploy");
    expect(row.classList.contains("attention")).toBe(true);
    expect(row.querySelector(".needs-attention")).not.toBeNull();
    expect(row.title).toContain("Claude asked for a person.");
    // No reminder to Claude about this step from the list: ▶ moves the run on.
    expect(row.querySelector("button.resume")).toBeNull();
    const go = row.querySelector<HTMLButtonElement>("button.go-on")!;
    expect(go.title).toBe('Go on from "Fix the tests" to the next step');
    go.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_continue").map((c) => c.args)).toEqual([{ runId: 5, stepId: "p", output: "done" }]);
    expect(callsOf("flow_resume")).toHaveLength(0);
    expect(callsOf("flow_load")).toHaveLength(0);
    expect(typed()).toEqual(["\x1b[200~Deploy it.\x1b[201~", "\r"]);
  });

  test("a run that needs attention at a step with several outputs offers them in the flow list", async () => {
    runPrompt = "Fix it.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "attention", note: "Claude asked for a person.", resumable: true })];
    await poll();
    const row = flowRow("Deploy");
    expect(row.querySelector("button.go-on")).toBeNull();
    const picks = [...row.querySelectorAll<HTMLButtonElement>(".picks button.pick")];
    expect(picks.map((b) => b.textContent)).toEqual(["pass", "fail"]);
    picks[1].click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_continue").map((c) => c.args)).toEqual([{ runId: 5, stepId: "p", output: "fail" }]);
    expect(typed()).toEqual(["\x1b[200~Fix it.\x1b[201~", "\r"]);
  });

  test("a run waiting at a manual step goes on from the flow list with the output picked there", async () => {
    runPrompt = "Ship it.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "manual", stepId: "m", step: "Review the diff", outputs: ["approve", "reject"] })];
    await poll();
    const picks = [...flowRow("Deploy").querySelectorAll<HTMLButtonElement>(".picks button.pick")];
    expect(picks.map((b) => b.textContent)).toEqual(["approve", "reject"]);
    expect(picks[0].title).toBe('Go on as "approve"');
    picks[0].click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_continue").map((c) => c.args)).toEqual([{ runId: 5, stepId: "m", output: "approve" }]);
    // Picking does not open the editor.
    expect(callsOf("flow_load")).toHaveLength(0);
    expect(typed()).toEqual(["\x1b[200~Ship it.\x1b[201~", "\r"]);
    // A running step is not offered there: Claude picks its output.
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    expect(flowRow("Deploy").querySelector(".picks")).toBeNull();
  });

  test("▶ on a manual step with one output finishes the step and goes on", async () => {
    runPrompt = "Submit the form.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "manual", stepId: "m", step: "Review the filled form", outputs: ["done"] })];
    await poll();
    const row = flowRow("Deploy");
    // ▶ does it, so there is no list of outputs; ■ still stops the run.
    expect(row.querySelector(".picks")).toBeNull();
    expect(row.querySelector("button.stop-run")).not.toBeNull();
    const go = row.querySelector<HTMLButtonElement>("button.go-on")!;
    expect(go.title).toBe('Done with "Review the filled form": go on');
    go.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_continue").map((c) => c.args)).toEqual([{ runId: 5, stepId: "m", output: "done" }]);
    expect(callsOf("flow_load")).toHaveLength(0);
    expect(typed()).toEqual(["\x1b[200~Submit the form.\x1b[201~", "\r"]);
  });

  test("a manual step with several outputs offers no ▶, as it cannot tell which", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "manual", stepId: "m", step: "Review", outputs: ["approve", "reject"] })];
    await poll();
    expect(flowRow("Deploy").querySelector("button.go-on")).toBeNull();
  });

  test("a pick in the flow list that cannot go on says why", async () => {
    runRefusal = "the run has moved on";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "manual", stepId: "m", step: "Review", outputs: ["ok"] })];
    await poll();
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.go-on")!.click();
    await settle();
    expect(box().querySelector<HTMLElement>(".error")!.textContent).toContain("the run has moved on");
  });

  test("Continue in the run bar has Claude go on with a step that needs attention", async () => {
    runPrompt = "Go on.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "attention", note: "Claude asked for a person.", resumable: true })];
    await poll();
    await open("Deploy");
    runBar().querySelector<HTMLButtonElement>("button.resume")!.click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_resume").map((c) => c.args)).toEqual([{ runId: 5, stepId: "p" }]);
    expect(typed()).toEqual(["\x1b[200~Go on.\x1b[201~", "\r"]);
  });

  test("■ stops the run", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.stop-run")!.click();
    await settle();
    expect(callsOf("flow_stop").map((c) => c.args)).toEqual([{ runId: 5 }]);
    expect(callsOf("flow_load")).toHaveLength(0);
    expect(running()).toEqual([]);
  });

  test("a running flow's editor shows its current step, and the outputs to go on by", async () => {
    runPrompt = "Deploy it.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, status: "attention", note: "Claude asked for a person." })];
    await poll();
    await open("Deploy");
    expect(current()).toEqual(["Fix the tests"]);
    expect(runBar().hidden).toBe(false);
    expect(runBar().textContent).toContain("Needs attention");
    expect(runBar().textContent).toContain("Claude asked for a person.");
    expect(outputs().map((b) => b.textContent)).toEqual(["pass", "fail"]);
    outputs()[0].click();
    await settle();
    await vi.advanceTimersByTimeAsync(200);
    expect(callsOf("flow_continue").map((c) => c.args)).toEqual([{ runId: 5, stepId: "p", output: "pass" }]);
    expect(typed()).toEqual(["\x1b[200~Deploy it.\x1b[201~", "\r"]);
  });

  test("the editor follows the run as it moves on, and ends", async () => {
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    activeRuns = [run({ tabId })];
    await poll();
    await open("Deploy");
    expect(current()).toEqual(["Fix the tests"]);
    activeRuns = [run({ tabId, stepId: "m", step: "Check the form", status: "manual", outputs: ["ok"], steps: 2 })];
    await poll();
    expect(current()).toEqual(["Check the form"]);
    expect(outputs().map((b) => b.textContent)).toEqual(["ok"]);
    activeRuns = [];
    await poll();
    expect(current()).toEqual([]);
    expect(runBar().hidden).toBe(true);
  });

  test("a running flow shows how many of its steps are done", async () => {
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    activeRuns = [run({ tabId, done: 1, total: 3 })];
    await poll();
    const progress = () => flowRow("Deploy").querySelector(".progress");
    expect(progress()?.textContent).toBe("1/3");
    expect(flowRow("Deploy").title).toContain("1 of 3 steps done");
    // A run whose steps cannot be read has nothing to count.
    activeRuns = [run({ tabId, done: 0, total: 0, note: "This run cannot be read" })];
    await poll();
    expect(progress()).toBeNull();
    expect(flowRow("Deploy").title).not.toContain("steps done");
    activeRuns = [];
    await poll();
    expect(progress()).toBeNull();
  });

  test("a run whose tab is gone shows on its flow, and ■ stops it", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: "gone" })];
    await poll();
    expect(running()).toEqual(["Deploy"]);
    expect(flowRow("Deploy").classList.contains("orphan")).toBe(true);
    expect(flowRow("Deploy").title).toContain("closed tab");
    // It cannot be answered, so the editor does not offer it.
    await open("Deploy");
    expect(runBar().hidden).toBe(true);
    flowRow("Deploy").querySelector<HTMLButtonElement>("button.stop-run")!.click();
    await settle();
    expect(callsOf("flow_stop").map((c) => c.args)).toEqual([{ runId: 5 }]);
  });

  test("closing a tab does not leave its run showing as one of a closed tab", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await flush();
    expect(running()).toEqual([]);
  });

  test("while the active tab runs a flow, ▶ on the others is off", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    const play = flowRow("Review").querySelector<HTMLButtonElement>("button.run")!;
    expect(play.disabled).toBe(true);
    expect(play.title).toContain("one flow at a time");
    activeRuns = [];
    await poll();
    expect(flowRow("Review").querySelector<HTMLButtonElement>("button.run")!.disabled).toBe(false);
  });

  test("the editor says when the run's step is no longer in the flow", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId, stepId: "old", step: "Old step" })];
    await poll();
    await open("Deploy");
    expect(current()).toEqual([]);
    expect(runBar().textContent).toContain("Old step");
    expect(runBar().textContent).toContain("no longer in this flow");
  });

  test("an open editor follows the active tab", async () => {
    await startApp();
    await openClaudeTabs(2);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    tabs()[0].click();
    await poll();
    await open("Deploy");
    expect(current()).toEqual(["Fix the tests"]);
    tabs()[1].click();
    expect(current()).toEqual([]);
    expect(runBar().hidden).toBe(true);
    tabs()[0].click();
    expect(current()).toEqual(["Fix the tests"]);
  });

  test("the run's step stays marked while the flow is edited", async () => {
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    await open("Deploy");
    editor().querySelector<HTMLButtonElement>("button.add-prompt")!.click();
    await settle();
    expect(current()).toEqual(["Fix the tests"]);
  });

  test("an editor shows only the active tab's run of its flow", async () => {
    await startApp();
    await openClaudeTabs(2);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    await open("Deploy");
    expect(current()).toEqual([]);
    expect(runBar().hidden).toBe(true);
  });
});

describe("flow runs and Claude's state", () => {
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const box = () => document.querySelector<HTMLElement>("#flows-box")!;
  const boxError = () => box().querySelector<HTMLElement>(".error")!;
  const output = () => document.querySelector<HTMLButtonElement>(".page.flow-editor.active .run-bar button.output")!;
  const runError = () => document.querySelector<HTMLElement>(".page.flow-editor.active .run-bar .run-error")!;
  const run = (over: Partial<Run> = {}): Run => ({
    id: 5, flowId: 1, flowName: "Deploy", tabId: "", stepId: "p", step: "Fix the tests",
    status: "manual", note: null, outputs: ["ok"], steps: 1, done: 0, total: 2, resumable: false, prefill: false, stepKind: "prompt", briefing: false, ...over,
  });
  async function poll() {
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    await settle();
  }
  async function open() {
    box().querySelector<HTMLElement>(".flows > li .name")!.click();
    await settle();
  }

  beforeEach(() => {
    storedFlows = [{ id: 1, name: "Deploy", archived: false }];
  });

  test("a prompt is only typed into a terminal tab while Claude still runs there", async () => {
    runPrompt = "Next.";
    await startApp();
    await clickTerminal();
    const tabId = spawns()[0].tabId;
    openClaudes = { 1: SESSION };
    activeRuns = [run({ tabId })];
    await poll();
    await open();
    // Claude has quit since the last check: the prompt would go to the shell.
    openClaudes = {};
    output().click();
    await settle();
    expect(callsOf("flow_continue")).toHaveLength(0);
    expect(callsOf("pty_write")).toHaveLength(0);
    // Said where it was clicked.
    expect(runError().textContent).toContain("Claude is not running");
    expect(runError().hidden).toBe(false);
    // With Claude back, it goes on.
    openClaudes = { 1: SESSION };
    output().click();
    await settle();
    expect(callsOf("flow_continue")).toHaveLength(1);
    expect(runError().hidden).toBe(true);
  });

  test("a run does not go on while Claude waits on background tasks, which wake it for another turn", async () => {
    runPrompt = "Next.";
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    activeRuns = [run({ tabId })];
    backgroundTabs = { [tabId]: Date.now() };
    await poll();
    await open();
    output().click();
    await settle();
    expect(callsOf("flow_continue")).toHaveLength(0);
    expect(runError().textContent).toContain("waiting on background tasks");
    backgroundTabs = {};
    await poll();
    output().click();
    await settle();
    expect(callsOf("flow_continue")).toHaveLength(1);
  });

  test("a prompt's Enter is held back if Claude left a terminal tab while it was pasted", async () => {
    runPrompt = "Next.";
    await startApp();
    await clickTerminal();
    const tabId = spawns()[0].tabId;
    openClaudes = { 1: SESSION };
    activeRuns = [run({ tabId })];
    await poll();
    await open();
    output().click();
    await settle();
    const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
    expect(typed()).toEqual(["\x1b[200~Next.\x1b[201~"]);
    // Suspended or quit: the shell, not Claude, would run the pasted line.
    openClaudes = {};
    await vi.advanceTimersByTimeAsync(200);
    await settle();
    expect(typed()).toEqual(["\x1b[200~Next.\x1b[201~"]);
  });

  test("a briefing that cannot be sent, as Claude left the tab, says so", async () => {
    runPrompt = "Fix it.";
    await startApp();
    await clickTerminal();
    openClaudes = { 1: SESSION };
    await poll();
    box().querySelector<HTMLButtonElement>("button.run")!.click();
    await settle();
    // Claude quits while the prompt is pasted: its Enter is held back, and the person told.
    openClaudes = {};
    await vi.advanceTimersByTimeAsync(200);
    await settle();
    expect(callsOf("flow_start")).toHaveLength(1);
    expect(callsOf("pty_write")).toHaveLength(1);
    // Nothing can move a run past an unsent briefing, so it is stopped.
    expect(boxError().textContent).toContain("was not sent");
    expect(boxError().textContent).toContain("was stopped");
    expect(callsOf("flow_stop").map((c) => c.args)).toEqual([{ runId: 5 }]);
  });

  test("Claude leaving before /clear is sent stops the run that would clear", async () => {
    storedGraphs[1] = { nodes: [{ ...START, clear: true }, { id: "p", kind: "prompt", title: "P", text: "x", outputs: ["done"], x: 0, y: 0 }], edges: [] };
    await startApp();
    await clickTerminal();
    openClaudes = { 1: SESSION };
    await poll();
    box().querySelector<HTMLButtonElement>("button.run")!.click();
    await settle();
    const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~"]);
    openClaudes = {};
    await vi.advanceTimersByTimeAsync(200);
    await settle();
    expect(typed()).toEqual(["\x1b[200~/clear\x1b[201~"]);
    expect(callsOf("flow_stop").map((c) => c.args)).toEqual([{ runId: 5 }]);
    expect(callsOf("flow_cleared")).toHaveLength(0);
    expect(boxError().textContent).toContain("before /clear was sent");
  });

  test("▶ checks a terminal tab's Claude right before starting", async () => {
    await startApp();
    await clickTerminal();
    openClaudes = { 1: SESSION };
    await poll();
    openClaudes = {};
    [...box().querySelectorAll<HTMLButtonElement>("button.run")][0].click();
    await settle();
    expect(callsOf("flow_start")).toHaveLength(0);
    expect(boxError().textContent).toContain("Claude tab");
  });

  test("closing a tab ends its run", async () => {
    await startApp();
    await openClaudeTabs(1);
    const tabId = spawns()[0].tabId;
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await settle();
    expect(callsOf("flow_stop_tab").map((c) => c.args)).toEqual([{ tabId }]);
  });

  test("runs that cannot be read are reported in the box until they can", async () => {
    await startApp();
    failing.add("flow_runs");
    await poll();
    expect(boxError().textContent).toContain("Flow runs could not be read");
    failing.delete("flow_runs");
    await poll();
    expect(boxError().hidden).toBe(true);
  });

  test("a refused output says why in the run bar", async () => {
    runRefusal = "the run has moved on";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    await open();
    output().click();
    await settle();
    expect(runError().textContent).toContain("the run has moved on");
    expect(output().disabled).toBe(false);
  });

  test("an output is sent once, however often it is clicked", async () => {
    runPrompt = "Next.";
    await startApp();
    await openClaudeTabs(1);
    activeRuns = [run({ tabId: spawns()[0].tabId })];
    await poll();
    await open();
    const ok = output();
    ok.click();
    ok.click();
    await settle();
    expect(callsOf("flow_continue")).toHaveLength(1);
  });
});

describe("session history", () => {
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
  const box = () => document.querySelector<HTMLElement>("#history-box")!;
  const rows = () => [...box().querySelectorAll<HTMLElement>(".sessions > li")];
  const titles = () => rows().map((li) => li.querySelector(".title")!.textContent);
  const hint = () => box().querySelector<HTMLElement>(".hint")!;
  const OTHER = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
  const THIRD = "9b2e1f0c-1d2e-4f3a-8b4c-5d6e7f8a9b0c";
  const FOURTH = "3d813cbb-47fb-42ba-91df-831e1593ac29";
  const past = (id: string, title: string, cwd = "/home/me/work"): Past => ({ id, title, cwd, modified: Date.now() - 60_000 });

  beforeEach(() => {
    pastSessions = [past(SESSION, "Fix the build"), past(OTHER, "Add tests")];
  });

  test("lists the Claude sessions of every folder, with their folders, whatever tab is active", async () => {
    pastSessions.push(past(THIRD, "Elsewhere", "/elsewhere"), past(FOURTH, "At home", "/home/me"));
    await startApp();
    await settle();
    expect(titles()).toEqual(["Fix the build", "Add tests", "Elsewhere", "At home"]);
    expect(rows().map((li) => li.querySelector(".folder")!.textContent)).toEqual(["work", "work", "elsewhere", "~"]);
    expect(callsOf("claude_history").at(-1)!.args).toEqual({ limit: 10 });
    expect(hint().hidden).toBe(true);
    await openClaudeTabs(1);
    liveCwds = { 1: "/elsewhere" };
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    await settle();
    expect(titles()).toEqual(["Fix the build", "Add tests", "Elsewhere", "At home"]);
  });

  test("says so when Claude has no sessions saved", async () => {
    pastSessions = [];
    await startApp();
    await settle();
    expect(hint().hidden).toBe(false);
    expect(hint().textContent).toContain("No Claude sessions");
  });

  test("clicking a session always resumes it in a new Claude tab in its folder", async () => {
    pastSessions.push(past(THIRD, "Elsewhere", "/elsewhere"));
    await startApp();
    await openClaudeTabs(1);
    await settle();
    rows()[1].querySelector<HTMLButtonElement>("button.resume")!.click();
    await settle();
    expect(spawns()[1]).toMatchObject({ kind: "claude", claudeSession: OTHER, cwd: "/home/me/work" });
    rows()[2].querySelector<HTMLButtonElement>("button.resume")!.click();
    await settle();
    expect(spawns()[2]).toMatchObject({ kind: "claude", claudeSession: THIRD, cwd: "/elsewhere" });
    expect(tabs()).toHaveLength(3);
    expect(typed()).toEqual([]);
  });

  test("the pin button keeps a session at the top, and unpins it again", async () => {
    pastSessions.push(past(THIRD, "Third"));
    await startApp();
    await settle();
    const pin = (i: number) => rows()[i].querySelector<HTMLButtonElement>("button.pin")!;
    expect(pin(2).getAttribute("aria-pressed")).toBe("false");
    pin(2).click();
    // One request at a time.
    expect(pin(2).disabled).toBe(true);
    await settle();
    expect(callsOf("claude_session_pin").at(-1)!.args).toEqual({ id: THIRD, pinned: true });
    expect(titles()).toEqual(["Third", "Fix the build", "Add tests"]);
    expect(pin(0).getAttribute("aria-pressed")).toBe("true");
    expect(spawns()).toHaveLength(0);
    pin(0).click();
    await settle();
    expect(callsOf("claude_session_pin").at(-1)!.args).toEqual({ id: THIRD, pinned: false });
    expect(titles()).toEqual(["Fix the build", "Add tests", "Third"]);
  });

  test("a failed pin is shown and changes nothing, and the next pin clears the notice", async () => {
    await startApp();
    await settle();
    const pin = (i: number) => rows()[i].querySelector<HTMLButtonElement>("button.pin")!;
    const reads = callsOf("claude_history").length;
    failing.add("claude_session_pin");
    pin(1).click();
    await settle();
    expect(noticeText()).toContain("could not be pinned");
    expect(callsOf("claude_history")).toHaveLength(reads);
    expect(titles()).toEqual(["Fix the build", "Add tests"]);
    expect(pin(1).getAttribute("aria-pressed")).toBe("false");
    expect(pin(1).disabled).toBe(false);
    failing.delete("claude_session_pin");
    pin(1).click();
    await settle();
    expect(titles()).toEqual(["Add tests", "Fix the build"]);
    expect(notice().textContent).not.toContain("could not be pinned");
  });

  test("an answer overtaken by a later read is dropped", async () => {
    await startApp();
    await settle();
    let open!: () => void;
    historyGates.push(new Promise((resolve) => (open = resolve)));
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    pastSessions = [past(THIRD, "Newer")];
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    await settle();
    expect(titles()).toEqual(["Newer"]);
    open();
    await settle();
    expect(titles()).toEqual(["Newer"]);
  });

  test("shows the newest 10, and Load more shows 10 more until there are none", async () => {
    const many = Array.from({ length: 25 }, (_, i) => past(`00000000-0000-4000-8000-${String(i).padStart(12, "0")}`, `p${i}`));
    pastSessions = many;
    await startApp();
    await openClaudeTabs(1);
    await settle();
    const more = () => box().querySelector<HTMLButtonElement>("button.load-more")!;
    expect(rows()).toHaveLength(10);
    expect(callsOf("claude_history").at(-1)!.args).toEqual({ limit: 10 });
    more().click();
    await settle();
    expect(rows()).toHaveLength(20);
    more().click();
    await settle();
    expect(rows()).toHaveLength(25);
    expect(more().hidden).toBe(true);
  });

  test("a session open in a tab is marked, and clicking it shows that tab instead of resuming", async () => {
    await startApp();
    await openClaudeTabs(2);
    tabSessions = { [spawns()[0].tabId]: OTHER, [spawns()[1].tabId]: SESSION };
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    await settle();
    expect(rows().map((li) => li.querySelector(".open")?.textContent)).toEqual(["this tab", "in work"]);
    rows()[1].querySelector<HTMLButtonElement>("button.resume")!.click();
    await settle();
    expect(tabs()[0].classList.contains("active")).toBe(true);
    expect(spawns()).toHaveLength(2);
    expect(typed()).toEqual([]);
  });

  test("a reload that changes nothing keeps the rows, and the focus in them", async () => {
    await startApp();
    await openClaudeTabs(1);
    await settle();
    const button = rows()[1].querySelector<HTMLButtonElement>("button.pin")!;
    button.focus();
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    await settle();
    expect(rows()[1].querySelector("button.pin")).toBe(button);
    expect(document.activeElement).toBe(button);
  });

  test("the box is read every 10 seconds only while it can be seen", async () => {
    await startApp();
    await openClaudeTabs(1);
    await settle();
    const reads = () => callsOf("claude_history").length;
    let before = reads();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(reads()).toBeGreaterThan(before);
    box().querySelector<HTMLButtonElement>(".toggle")!.click();
    before = reads();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(reads()).toBe(before);
    // Shown again, it is read at once.
    box().querySelector<HTMLButtonElement>(".toggle")!.click();
    await settle();
    expect(reads()).toBe(before + 1);
    document.querySelector<HTMLButtonElement>("#toggle-sidebar")!.click();
    before = reads();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(reads()).toBe(before);
  });

  test("a failed Load more keeps the sessions shown, and the next try asks for the same page", async () => {
    pastSessions = Array.from({ length: 25 }, (_, i) => past(`00000000-0000-4000-8000-${String(i).padStart(12, "0")}`, `p${i}`));
    await startApp();
    await openClaudeTabs(1);
    await settle();
    failing.add("claude_history");
    box().querySelector<HTMLButtonElement>("button.load-more")!.click();
    await settle();
    expect(rows()).toHaveLength(10);
    expect(box().querySelector<HTMLElement>(".error")!.hidden).toBe(false);
    failing.delete("claude_history");
    box().querySelector<HTMLButtonElement>("button.load-more")!.click();
    await settle();
    expect(callsOf("claude_history").at(-1)!.args).toMatchObject({ limit: 20 });
    expect(rows()).toHaveLength(20);
    expect(box().querySelector<HTMLElement>(".error")!.hidden).toBe(true);
  });

  test("Load more waits for its page before it can be clicked again", async () => {
    pastSessions = Array.from({ length: 25 }, (_, i) => past(`00000000-0000-4000-8000-${String(i).padStart(12, "0")}`, `p${i}`));
    await startApp();
    await openClaudeTabs(1);
    await settle();
    const more = () => box().querySelector<HTMLButtonElement>("button.load-more")!;
    let open!: () => void;
    historyGates.push(new Promise((resolve) => (open = resolve)));
    more().click();
    expect(more().disabled).toBe(true);
    more().click();
    open();
    await settle();
    expect(callsOf("claude_history").at(-1)!.args).toEqual({ limit: 20 });
    expect(rows()).toHaveLength(20);
    expect(more().disabled).toBe(false);
  });

  test("a read that keeps failing is logged once, not on every poll", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await startApp();
    await openClaudeTabs(1);
    await settle();
    failing.add("claude_history");
    failing.add("claude_tab_sessions");
    await vi.advanceTimersByTimeAsync(30_000);
    const logged = (text: string) => error.mock.calls.filter((c) => String(c[0]).includes(text)).length;
    expect(logged("could not read the Claude sessions")).toBe(1);
    expect(logged("could not read the tabs' Claude sessions")).toBe(1);
    // Logged again once it failed anew after working.
    failing.delete("claude_history");
    failing.delete("claude_tab_sessions");
    await vi.advanceTimersByTimeAsync(10_000);
    failing.add("claude_history");
    failing.add("claude_tab_sessions");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(logged("could not read the Claude sessions")).toBe(2);
    expect(logged("could not read the tabs' Claude sessions")).toBe(2);
  });

  test("transcripts that could not be read are counted in the box", async () => {
    historyUnreadable = 2;
    await startApp();
    await openClaudeTabs(1);
    await settle();
    const note = box().querySelector<HTMLElement>(".unreadable")!;
    expect(note.hidden).toBe(false);
    expect(note.textContent).toContain("2 sessions could not be read");
  });

  test("the refresh button reads the sessions again, and a failure is shown", async () => {
    await startApp();
    await openClaudeTabs(1);
    await settle();
    pastSessions.push(past(THIRD, "Third"));
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    await settle();
    expect(titles()).toEqual(["Fix the build", "Add tests", "Third"]);
    failing.add("claude_history");
    box().querySelector<HTMLButtonElement>(".refresh")!.click();
    await settle();
    expect(box().querySelector<HTMLElement>(".error")!.hidden).toBe(false);
  });
});

describe("tab order", () => {
  const mouse = (target: EventTarget, type: string, clientX: number) =>
    target.dispatchEvent(new MouseEvent(type, { bubbles: true, button: 0, clientX }));

  beforeEach(() => {
    // Each tab is 100px wide, side by side in its current order from 0.
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function (this: HTMLElement) {
      return Math.max(tabs().indexOf(this), 0) * 100;
    });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(100);
  });

  /** Drags tab `i` to `x`, then releases it, with the click a browser sends after. */
  function drag(i: number, x: number) {
    const tab = tabs()[i];
    const start = i * 100 + 10;
    mouse(tab.querySelector(".name")!, "mousedown", start);
    mouse(document, "mousemove", start + 2);
    mouse(document, "mousemove", x);
    mouse(document, "mouseup", x);
    tab.click();
  }

  test("a tab dragged along the tab bar moves there and is saved in its new place", async () => {
    await startApp();
    for (let i = 0; i < 3; i++) await clickTerminal();
    drag(2, 10);
    expect(tabNames()).toEqual(["shell 3", "shell 1", "shell 2"]);
    // Let go, it settles into its place.
    expect(tabs()[0].style.transform).toBe("");
    drag(1, 290);
    expect(tabNames()).toEqual(["shell 3", "shell 2", "shell 1"]);
    // The tab clicked last is the one let go last.
    expect(tabs()[2].classList.contains("active")).toBe(true);
    await win.closeRequested!();
    expect(lastSaved()!.tabs.map((t) => t.name)).toEqual(["shell 3", "shell 2", "shell 1"]);
    expect(lastSaved()!.active).toBe(2);
  });

  test("a terminal tab stays before the page tabs", async () => {
    await startApp();
    for (let i = 0; i < 2; i++) await clickTerminal();
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 6; i++) await flush();
    drag(0, 290);
    expect(tabNames()).toEqual(["shell 2", "shell 1", "Settings"]);
  });

  test("a tab grabbed again while it settles stays marked as dragged", async () => {
    // Like the Web Animations API: a cancelled animation reports it later.
    const running = new Map<Element, Animation[]>();
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    proto.animate = function (this: HTMLElement) {
      const a = {
        onfinish: null as ((e: AnimationPlaybackEvent) => void) | null,
        oncancel: null as ((e: AnimationPlaybackEvent) => void) | null,
        cancel() {
          queueMicrotask(() => a.oncancel?.(new Event("cancel") as AnimationPlaybackEvent));
        },
      } as unknown as Animation;
      running.set(this, [...(running.get(this) ?? []), a]);
      return a;
    };
    proto.getAnimations = function (this: HTMLElement) {
      const list = running.get(this) ?? [];
      running.delete(this);
      return list;
    };
    try {
      await startApp();
      for (let i = 0; i < 3; i++) await clickTerminal();
      // Let go 20px off its place, it slides back into it.
      drag(2, 30);
      mouse(tabs()[0].querySelector(".name")!, "mousedown", 30);
      mouse(document, "mousemove", 40);
      await flush();
      expect(tabs()[0].classList.contains("dragging")).toBe(true);
      mouse(document, "mouseup", 40);
    } finally {
      delete proto.animate;
      delete proto.getAnimations;
    }
  });

  test("a drag ends where it is when the tab bar is drawn again", async () => {
    await startApp();
    for (let i = 0; i < 2; i++) await clickTerminal();
    mouse(tabs()[0].querySelector(".name")!, "mousedown", 10);
    mouse(document, "mousemove", 20);
    expect(document.body.classList.contains("dragging-tab")).toBe(true);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 6; i++) await flush();
    mouse(document, "mousemove", 190);
    expect(document.body.classList.contains("dragging-tab")).toBe(false);
    expect(tabNames()).toEqual(["shell 1", "shell 2", "Settings"]);
    // The drag no longer follows the mouse.
    mouse(document, "mousemove", 190);
    expect(tabNames()).toEqual(["shell 1", "shell 2", "Settings"]);
    expect(tabs().some((t) => t.classList.contains("dragging"))).toBe(false);
  });

  test("pressing the close button or a short click does not move the tab", async () => {
    await startApp();
    for (let i = 0; i < 2; i++) await clickTerminal();
    mouse(tabs()[0].querySelector(".x")!, "mousedown", 90);
    mouse(document, "mousemove", 190);
    mouse(document, "mouseup", 190);
    expect(tabNames()).toEqual(["shell 1", "shell 2"]);
    mouse(tabs()[1], "mousedown", 110);
    mouse(document, "mouseup", 111);
    tabs()[1].click();
    expect(tabNames()).toEqual(["shell 1", "shell 2"]);
    expect(tabs()[1].classList.contains("active")).toBe(true);
  });
});

describe("toolbox order", () => {
  const order = () => [...document.querySelectorAll<HTMLElement>("#sidebar .toolbox")].map((b) => b.id);
  const header = (id: string) => document.querySelector<HTMLElement>(`#${id} header .toggle`)!;
  const mouse = (target: EventTarget, type: string, clientY: number) =>
    target.dispatchEvent(new MouseEvent(type, { bubbles: true, button: 0, clientY }));

  beforeEach(() => {
    // Each toolbox is 100px tall, stacked in its current order from the sidebar's top at 0.
    const top = (el: HTMLElement) => Math.max([...document.querySelectorAll("#sidebar .toolbox")].indexOf(el), 0) * 100;
    vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(function (this: HTMLElement) {
      return top(this);
    });
    vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(100);
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
      const t = this.id === "sidebar" ? 0 : top(this);
      return { top: t, bottom: t + 100, height: 100, left: 0, right: 200, width: 200, x: 0, y: t, toJSON() {} } as DOMRect;
    });
  });

  /** Drags a toolbox by its header to `y`, then releases it, with the click a browser sends after. */
  function drag(id: string, y: number) {
    const start = order().indexOf(id) * 100 + 10;
    mouse(header(id), "mousedown", start);
    mouse(document, "mousemove", start + 2);
    mouse(document, "mousemove", y);
    mouse(document, "mouseup", y);
    header(id).click();
  }

  test("a toolbox dragged by its header moves, without collapsing, and the order is remembered", async () => {
    await startApp();
    expect(order()).toEqual(["workspace-box", "flows-box", "history-box"]);
    drag("history-box", 10);
    expect(order()).toEqual(["history-box", "workspace-box", "flows-box"]);
    expect(document.querySelector("#history-box")!.classList.contains("collapsed")).toBe(false);
    drag("workspace-box", 290);
    expect(order()).toEqual(["history-box", "flows-box", "workspace-box"]);
    // Let go, it settles into its place.
    expect(document.querySelector<HTMLElement>("#workspace-box")!.style.transform).toBe("");
    // A fresh app instance on a fresh page.
    vi.resetModules();
    await startApp();
    expect(order()).toEqual(["history-box", "flows-box", "workspace-box"]);
  });

  test("Alt+Up and Alt+Down on a header move its toolbox, and the order is remembered", async () => {
    await startApp();
    const keyOn = (id: string, key: string) =>
      header(id).dispatchEvent(new KeyboardEvent("keydown", { key, altKey: true, bubbles: true }));
    keyOn("history-box", "ArrowUp");
    expect(order()).toEqual(["workspace-box", "history-box", "flows-box"]);
    keyOn("workspace-box", "ArrowDown");
    expect(order()).toEqual(["history-box", "workspace-box", "flows-box"]);
    expect(document.activeElement).toBe(header("workspace-box"));
    vi.resetModules();
    await startApp();
    expect(order()).toEqual(["history-box", "workspace-box", "flows-box"]);
  });

  test("Reset sidebar on the Settings page restores the order and opens every toolbox", async () => {
    await startApp();
    drag("history-box", 10);
    header("flows-box").click();
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    for (let i = 0; i < 6; i++) await flush();
    document.querySelector<HTMLButtonElement>("#settings .reset-sidebar")!.click();
    expect(order()).toEqual(["workspace-box", "flows-box", "history-box"]);
    expect(document.querySelector("#flows-box")!.classList.contains("collapsed")).toBe(false);
    vi.resetModules();
    await startApp();
    expect(order()).toEqual(["workspace-box", "flows-box", "history-box"]);
    expect(document.querySelector("#flows-box")!.classList.contains("collapsed")).toBe(false);
  });

  test("a drag released outside the window does not swallow a later click", async () => {
    await startApp();
    const start = 210;
    mouse(header("history-box"), "mousedown", start);
    mouse(document, "mousemove", start + 2);
    mouse(document, "mousemove", 10);
    mouse(document, "mouseup", 10);
    // No click follows; a while later the person clicks a header.
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 5_000);
    header("flows-box").click();
    expect(document.querySelector("#flows-box")!.classList.contains("collapsed")).toBe(true);
  });

  test("a click without a drag still collapses", async () => {
    await startApp();
    mouse(header("flows-box"), "mousedown", 110);
    mouse(document, "mouseup", 111);
    header("flows-box").click();
    expect(document.querySelector("#flows-box")!.classList.contains("collapsed")).toBe(true);
    expect(order()).toEqual(["workspace-box", "flows-box", "history-box"]);
  });

  test("a stored order with unknown or missing toolboxes keeps the others in place", async () => {
    localStorage.setItem("misag.toolboxOrder", JSON.stringify(["flows-box", "gone-box"]));
    await startApp();
    expect(order()).toEqual(["flows-box", "workspace-box", "history-box"]);
    vi.resetModules();
    localStorage.setItem("misag.toolboxOrder", "not json");
    await startApp();
    expect(order()).toEqual(["workspace-box", "flows-box", "history-box"]);
  });
});

describe("sidebar width", () => {
  const sidebar = () => document.querySelector<HTMLElement>("#sidebar")!;
  const resizer = () => document.querySelector<HTMLElement>("#sidebar-resizer")!;
  const width = () => sidebar().style.getPropertyValue("--sidebar-width");
  const stored = () => localStorage.getItem("misag.sidebarWidth");
  /** A mouse event at `clientX`; a move holds the left button unless `buttons` says otherwise. */
  const mouse = (target: EventTarget, type: string, clientX: number, init: MouseEventInit = {}) =>
    target.dispatchEvent(
      new MouseEvent(type, { bubbles: true, cancelable: true, button: 0, buttons: type === "mousemove" ? 1 : 0, clientX, ...init }),
    );
  const key = (k: string) => {
    const e = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true });
    resizer().dispatchEvent(e);
    return e;
  };
  const setWindowWidth = (px: number) => Object.defineProperty(window, "innerWidth", { value: px, configurable: true, writable: true });
  /** Drags the sidebar's edge from `from` (where it is now by default) to `x`, then releases it. */
  function drag(x: number, from = 240) {
    mouse(resizer(), "mousedown", from);
    mouse(document, "mousemove", x);
    mouse(document, "mouseup", x);
  }

  beforeEach(() => setWindowWidth(1024));
  afterEach(() => setWindowWidth(1024));

  test("dragging the sidebar's edge resizes it, and the width is remembered", async () => {
    await startApp();
    expect(width()).toBe("240px");
    drag(320);
    expect(width()).toBe("320px");
    expect(document.body.classList.contains("resizing-sidebar")).toBe(false);
    vi.resetModules();
    await startApp();
    expect(width()).toBe("320px");
  });

  test("the edge moves by as far as the mouse went, wherever it was grabbed", async () => {
    await startApp();
    drag(263, 243);
    expect(width()).toBe("260px");
    drag(282, 262);
    expect(width()).toBe("280px");
  });

  test("the width is saved once let go, and moving the mouse after that leaves it", async () => {
    await startApp();
    mouse(resizer(), "mousedown", 240);
    mouse(document, "mousemove", 320);
    expect(width()).toBe("320px");
    expect(stored()).toBeNull();
    mouse(document, "mouseup", 320);
    expect(stored()).toBe("320");
    mouse(document, "mousemove", 500);
    expect(width()).toBe("320px");
  });

  test("a drag let go outside the window ends at the next move without the button, or when the window loses focus", async () => {
    await startApp();
    mouse(resizer(), "mousedown", 240);
    mouse(document, "mousemove", 300);
    mouse(document, "mousemove", 400, { buttons: 0 });
    expect(width()).toBe("300px");
    expect(stored()).toBe("300");
    expect(document.body.classList.contains("resizing-sidebar")).toBe(false);

    mouse(resizer(), "mousedown", 300);
    mouse(document, "mousemove", 350);
    window.dispatchEvent(new Event("blur"));
    mouse(document, "mousemove", 450);
    expect(width()).toBe("350px");
    expect(stored()).toBe("350");
    expect(document.body.classList.contains("resizing-sidebar")).toBe(false);
  });

  test("only the left button drags the edge", async () => {
    await startApp();
    mouse(resizer(), "mousedown", 240, { button: 2 });
    mouse(document, "mousemove", 320);
    expect(width()).toBe("240px");
    expect(document.body.classList.contains("resizing-sidebar")).toBe(false);
  });

  test("the terminal is fitted once the edge is let go, not at every move", async () => {
    await startApp();
    await clickClaude();
    const term = terms()[0];
    const before = term.fits;
    mouse(resizer(), "mousedown", 240);
    mouse(document, "mousemove", 300);
    for (const cb of resizeObservers) cb([], {} as ResizeObserver);
    mouse(document, "mousemove", 360);
    for (const cb of resizeObservers) cb([], {} as ResizeObserver);
    expect(term.fits).toBe(before);
    mouse(document, "mouseup", 360);
    expect(term.fits).toBe(before + 1);
  });

  test("the sidebar stays between its narrowest and widest", async () => {
    await startApp();
    drag(10);
    expect(width()).toBe("160px");
    drag(5000, 160);
    expect(width()).toBe("600px");
  });

  test("the sidebar leaves the terminals room in a narrow window, and takes its width back when the window grows", async () => {
    localStorage.setItem("misag.sidebarWidth", "500");
    await startApp();
    expect(width()).toBe("500px");
    setWindowWidth(700);
    window.dispatchEvent(new Event("resize"));
    expect(width()).toBe("380px");
    expect(resizer().getAttribute("aria-valuemax")).toBe("380");
    expect(stored()).toBe("500");
    setWindowWidth(1024);
    window.dispatchEvent(new Event("resize"));
    expect(width()).toBe("500px");
    setWindowWidth(700);
    window.dispatchEvent(new Event("resize"));
    drag(1000, 380);
    expect(width()).toBe("380px");
  });

  test("the arrow keys on the sidebar's edge resize it, and a double-click restores the default", async () => {
    await startApp();
    key("ArrowRight");
    expect(width()).toBe("256px");
    key("ArrowLeft");
    key("ArrowLeft");
    expect(width()).toBe("224px");
    expect(resizer().getAttribute("aria-valuenow")).toBe("224");
    expect(resizer().getAttribute("aria-valuetext")).toBe("224 pixels");
    expect(key("a").defaultPrevented).toBe(false);
    expect(width()).toBe("224px");
    key("End");
    expect(width()).toBe("600px");
    key("Home");
    expect(width()).toBe("160px");
    resizer().dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    expect(width()).toBe("240px");
    vi.resetModules();
    await startApp();
    expect(width()).toBe("240px");
  });

  test("Reset sidebar on the Settings page restores the default width", async () => {
    await startApp();
    drag(400);
    document.querySelector<HTMLButtonElement>("#settings .reset-sidebar")!.click();
    expect(width()).toBe("240px");
    vi.resetModules();
    await startApp();
    expect(width()).toBe("240px");
  });

  test.each([
    ["wide", "240px"],
    ["", "240px"],
    ["9999", "600px"],
    ["10", "160px"],
  ])("a stored width of %j opens the sidebar at %s", async (value, expected) => {
    localStorage.setItem("misag.sidebarWidth", value);
    await startApp();
    expect(width()).toBe(expected);
  });
});

describe("dropping files on the window", () => {
  const settle = async () => {
    for (let i = 0; i < 6; i++) await flush();
  };
  const typed = () => callsOf("pty_write").map((c) => new TextDecoder().decode(new Uint8Array(c.args.data as number[])));
  const dropFiles = async (paths: string[]) => {
    drop.emit!({ type: "drop", paths });
    await settle();
  };

  test("types the dropped paths into the active tab, shell-quoted on one line, and brings the window to the front", async () => {
    await startApp();
    await openClaudeTabs(1);
    await dropFiles(["/home/u/a.txt", "/home/u/my notes.txt"]);
    expect(typed()).toEqual(["/home/u/a.txt '/home/u/my notes.txt' "]);
    expect(win.focused).toBe(1);
  });

  test("types them as a bracketed paste when the program in the tab asked for one", async () => {
    await startApp();
    await openClaudeTabs(1);
    terms()[0].bracketed = true;
    await dropFiles(["/home/u/a.txt"]);
    expect(typed()).toEqual(["\x1b[200~/home/u/a.txt \x1b[201~"]);
  });

  test("ignores dragging over the window, leaving it and an empty drop", async () => {
    await startApp();
    await openClaudeTabs(1);
    drop.emit!({ type: "enter", paths: ["/home/u/a.txt"] });
    drop.emit!({ type: "over" });
    drop.emit!({ type: "leave" });
    await dropFiles([]);
    expect(callsOf("pty_write")).toHaveLength(0);
    expect(noticeVisible()).toBe(false);
  });

  test("one path that could act as keys types none, and the notice says so", async () => {
    await startApp();
    await openClaudeTabs(1);
    await dropFiles(["/home/u/b\rc", "/home/u/a.txt"]);
    expect(noticeText()).toContain('"/home/u/b\\rc" was not typed: its name contains control characters');
    expect(noticeText()).toContain("none of the 2 dropped files were");
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("links and files that are not local are not typed", async () => {
    await startApp();
    await openClaudeTabs(1);
    await dropFiles(["/home/u/a.txt", "https://example.com/x"]);
    expect(noticeText()).toContain('"https://example.com/x" was not typed: it is not a local file');
    await dropFiles(["/home/u/\ufffd.txt"]);
    expect(noticeText()).toContain("its name could not be read");
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("too many files at once are not typed, and the notice names only the first", async () => {
    await startApp();
    await openClaudeTabs(1);
    await dropFiles(Array.from({ length: 101 }, (_, i) => `/home/u/f${i}`));
    expect(noticeText()).toContain('"/home/u/f0" and 100 more were not typed: more than 100 files were dropped.');
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("without a running tab, a notice says so", async () => {
    await startApp();
    await dropFiles(["/home/u/a.txt", "/home/u/b.txt"]);
    expect(noticeText()).toContain('"/home/u/a.txt" and 1 more were not typed: no tab is running.');
  });

  test("a drop while a page is shown is not typed, and says where to drop", async () => {
    await startApp();
    await openClaudeTabs(1);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    await settle();
    await dropFiles(["/home/u/a.txt"]);
    expect(callsOf("pty_write")).toHaveLength(0);
    expect(noticeText()).toContain("Drop files on a terminal tab to type their paths.");
  });

  test("when drops cannot be watched, a notice says so", async () => {
    drop.fails = true;
    await startApp();
    expect(noticeText()).toContain("Dropping files on the window will not type their paths");
  });
});

describe("startup and dispose", () => {
  const HISTORY_POLL_MS = 60_000;
  /** Leaves the page as index.html, without starting the app. */
  const loadPage = () => (document.body.innerHTML = new DOMParser().parseFromString(indexHtml, "text/html").body.innerHTML);

  test.each([
    ["the page", loadPage],
    ["an empty body", () => (document.body.innerHTML = "")],
  ])("importing main.ts on %s starts nothing and renders nothing", async (_, setUp) => {
    setUp();
    const before = document.body.innerHTML;
    await import("./main");
    await flush();
    expect(vi.getTimerCount()).toBe(0);
    expect(calls).toEqual([]);
    expect(document.body.innerHTML).toBe(before);
  });

  test("dispose stops the timers, listeners and pending callbacks", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    // A pending save, a finish settling, and the hook report check are all waiting.
    renameTab(0, "build");
    terms()[0].onTitle!("◐ Claude Code");
    terms()[0].onTitle!("✳ Claude Code");
    terms()[0].element.dispatchEvent(new MouseEvent("mouseup", { button: 0, bubbles: true }));
    dispose!();
    dispose = null;
    await flush();
    expect(win.unlistened).toBe(1);
    expect(drop.unlistened).toBe(1);
    expect(resizeDisconnects).toBe(1);
    const before = calls.length;
    const primed = sounds.primed;
    const width = document.querySelector<HTMLElement>("#sidebar")!.style.getPropertyValue("--sidebar-width");
    await vi.advanceTimersByTimeAsync(HISTORY_POLL_MS * 2);
    expect(vi.getTimerCount()).toBe(0);
    Object.defineProperty(window, "innerWidth", { value: 300, configurable: true, writable: true });
    window.dispatchEvent(new Event("resize"));
    Object.defineProperty(window, "innerWidth", { value: 1024, configurable: true, writable: true });
    document.dispatchEvent(new Event("visibilitychange"));
    tabs()[0].dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    await flush();
    expect(calls.slice(before)).toEqual([]);
    expect(sounds.played).toEqual([]);
    expect(sounds.primed).toBe(primed);
    expect(document.querySelector<HTMLElement>("#sidebar")!.style.getPropertyValue("--sidebar-width")).toBe(width);
  });

  test("the Tauri listeners are removed even when they are added after dispose", async () => {
    loadPage();
    const main = await import("./main");
    main.boot()();
    await flush();
    expect(win.unlistened).toBe(1);
    expect(drop.unlistened).toBe(1);
  });

  test("startup work that finishes after dispose changes nothing", async () => {
    savedLayout = { tabs: [{ tabId: "a", kind: "terminal", name: "shell 1", renamed: false, cwd: null, command: null, claudeSession: null }], active: 0 };
    let release!: () => void;
    settingsLoadGate = new Promise((r) => (release = r));
    loadPage();
    const main = await import("./main");
    main.boot()();
    release();
    for (let i = 0; i < 5; i++) await flush();
    expect(callsOf("pty_spawn")).toEqual([]);
    expect(callsOf("fs_list")).toEqual([]);
    expect(tabs()).toEqual([]);
    expect(vi.getTimerCount()).toBe(0);
  });

  test("a tab check in flight at dispose changes nothing once it returns", async () => {
    storedSettings = { ...DEFAULTS, finishSound: "chime" };
    await startApp();
    await clickClaude();
    await clickTerminal();
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    // The held check would confirm the Claude tab's finish and show Claude in the terminal tab.
    terms()[0].onTitle!("◐ Claude Code");
    terms()[0].onTitle!("✳ Claude Code");
    await vi.advanceTimersByTimeAsync(1_000);
    let release!: () => void;
    claudeCheckGate = new Promise((r) => (release = r));
    openClaudes[2] = SESSION;
    const checks = callsOf("claude_open_sessions").length;
    await vi.advanceTimersByTimeAsync(TAB_POLL_MS);
    expect(callsOf("claude_open_sessions").length).toBe(checks + 1);
    const bar = document.querySelector("#tabs")!.innerHTML;
    const before = calls.length;
    dispose!();
    dispose = null;
    release();
    for (let i = 0; i < 5; i++) await flush();
    expect(sounds.played).toEqual([]);
    expect(calls.slice(before)).toEqual([]);
    expect(document.querySelector("#tabs")!.innerHTML).toBe(bar);
  });

  test("a boot that throws partway stops what it started, so the page can boot again", async () => {
    loadPage();
    document.querySelector("#flows-box")!.remove();
    const main = await import("./main");
    expect(() => main.boot()).toThrow("misag: missing element #flows-box");
    expect(resizeDisconnects).toBe(1);
    const width = document.querySelector<HTMLElement>("#sidebar")!.style.getPropertyValue("--sidebar-width");
    Object.defineProperty(window, "innerWidth", { value: 300, configurable: true, writable: true });
    window.dispatchEvent(new Event("resize"));
    Object.defineProperty(window, "innerWidth", { value: 1024, configurable: true, writable: true });
    expect(document.querySelector<HTMLElement>("#sidebar")!.style.getPropertyValue("--sidebar-width")).toBe(width);
    await flush();
    expect(vi.getTimerCount()).toBe(0);
    await startApp();
    await clickTerminal();
    expect(tabNames()).toEqual(["shell 1"]);
  });

  test("booting twice without dispose throws, and booting again after dispose works", async () => {
    await startApp();
    const main = await import("./main");
    expect(() => main.boot()).toThrow("misag: boot called twice");
    await startApp();
    await clickTerminal();
    expect(tabNames()).toEqual(["shell 1"]);
  });
});
