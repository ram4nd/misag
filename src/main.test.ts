import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import type { Channel } from "@tauri-apps/api/core";
import indexHtml from "../index.html?raw";
import capabilities from "../src-tauri/capabilities/default.json?raw";
import sharedLayout from "../src-tauri/tests/fixtures/layout.json?raw";

// xterm needs a real canvas and layout, so the tests drive a stand-in that
// records writes and exposes the handlers main.ts registers.
const fakes = vi.hoisted(() => {
  type Handler = (...args: any[]) => void;
  class FakeTerminal {
    static all: FakeTerminal[] = [];
    /** Makes the next terminal's open() throw, as a broken view would. */
    static failOnce = false;
    cols = 80;
    rows = 24;
    written: string[] = [];
    handlers: Record<string, Handler> = {};
    disposed = false;
    options: Record<string, unknown>;
    constructor(options: Record<string, unknown> = {}) {
      this.options = { ...options };
      FakeTerminal.all.push(this);
    }
    get output() {
      return this.written.join("");
    }
    write(data: string | Uint8Array) {
      this.written.push(typeof data === "string" ? data : new TextDecoder().decode(data));
    }
    private on(name: string) {
      return (h: Handler) => {
        this.handlers[name] = h;
        return { dispose() {} };
      };
    }
    onData = this.on("data");
    onBinary = this.on("binary");
    onResize = this.on("resize");
    onTitleChange = this.on("title");
    loadAddon() {}
    open() {
      if (FakeTerminal.failOnce) {
        FakeTerminal.failOnce = false;
        throw new Error("no canvas");
      }
    }
    focus() {}
    reset() {
      this.written = [];
    }
    dispose() {
      this.disposed = true;
    }
  }
  class FakeWebLinksAddon {
    static all: FakeWebLinksAddon[] = [];
    constructor(public handler: (e: unknown, uri: string) => void) {
      FakeWebLinksAddon.all.push(this);
    }
  }
  return { FakeTerminal, FakeWebLinksAddon };
});

// The window is faked directly: the IPC mocks cannot deliver window events.
const win = vi.hoisted(() => ({
  closeRequested: undefined as undefined | (() => Promise<unknown>),
  /** Whether the last close request was cancelled. */
  prevented: false,
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: async (handler: (event: { preventDefault(): void }) => Promise<unknown>) => {
      win.closeRequested = () => {
        win.prevented = false;
        return handler({ preventDefault: () => (win.prevented = true) });
      };
      return () => {};
    },
  }),
}));

// The folder picker answers with `picker.picked` (null: cancelled), or fails if `picker.fails`.
const picker = vi.hoisted(() => ({
  picked: null as string | null,
  fails: false,
  calls: [] as { defaultPath?: string }[],
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: async (options: { defaultPath?: string }) => {
    picker.calls.push(options);
    if (picker.fails) throw new Error("no portal");
    return picker.picked;
  },
}));

vi.mock("@xterm/xterm/css/xterm.css", () => ({}));
vi.mock("@xterm/xterm", () => ({ Terminal: fakes.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: fakes.FakeWebLinksAddon }));

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

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const SESSION = "0f8fad5b-d9cb-469f-a165-70867728950e";
const LOG = "/home/me/.local/share/com.tag1.raterm/hook.log";

let calls: Call[];
let spawnBehaviour: (args: SpawnCall, id: number) => number | Promise<number>;
let savedLayout: Layout;
let liveCwds: Record<number, string>;
/** The Claude session claude_open_sessions finds open, by PTY id; other tabs are closed unless in `claudeStates`. */
let openClaudes: Record<number, string>;
/** Other answers of claude_open_sessions, by PTY id. */
let claudeStates: Record<number, { state: "unknown" } | { state: "untracked"; pid: number }>;
/** What pty_spawn reports about a terminal tab's saved Claude session. */
let resumeReport: { state: "typed" } | { state: "noConversation" } | { state: "failed"; message: string };
let failing: Set<string>;
let storeReport: {
  store: { state: "saving" } | { state: "recovered"; notice: string } | { state: "notSaving"; reason: string };
  hookProblem: { at: number; message: string; log: string } | null;
  wrapperProblem?: string | null;
};
let reported: Set<string>;
let missingDirs: Set<string>;
type Settings = Record<string, unknown>;
let storedSettings: Settings;
/** fs_list for a directory waits for its promise here, so a test can hold it back. */
let fsGates: Record<string, Promise<void>>;
/** How many hidden entries fs_list reports per directory. */
let fsHidden: Record<string, number>;
/** What fs_list returns per directory; any other directory fails. */
let fsTree: Record<string, { name: string; dir: boolean }[]>;
/** settings_load waits for this, so a test can hold the load back. */
let settingsLoadGate: Promise<void>;
const DEFAULTS: Settings = { skipPermissions: false, workspace: null };

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

async function boot() {
  document.body.innerHTML = new DOMParser().parseFromString(indexHtml, "text/html").body.innerHTML;
  await import("./main");
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

beforeEach(() => {
  // Fake timers for every test: they are discarded afterwards, so a debounced save
  // left over from one test's app instance cannot fire during the next test.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
  vi.resetModules();
  fakes.FakeTerminal.all = [];
  fakes.FakeTerminal.failOnce = false;
  fakes.FakeWebLinksAddon.all = [];
  calls = [];
  let nextId = 0;
  spawnBehaviour = (_args, id) => id;
  savedLayout = { tabs: [], active: 0 };
  liveCwds = {};
  openClaudes = {};
  claudeStates = {};
  resumeReport = { state: "typed" };
  failing = new Set();
  storeReport = { store: { state: "saving" }, hookProblem: null };
  reported = new Set();
  missingDirs = new Set();
  storedSettings = { ...DEFAULTS };
  settingsLoadGate = Promise.resolve();
  fsTree = { "~": [{ name: "work", dir: true }] };
  picker.picked = null;
  picker.fails = false;
  picker.calls = [];
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
    if (cmd === "claude_open_sessions") {
      const ids = (args as { tabs: { id: number }[] }).tabs.map((t) => t.id);
      return Object.fromEntries(
        ids.map((id) => [
          id,
          id in openClaudes ? { state: "open", session: openClaudes[id] } : (claudeStates[id] ?? { state: "closed" }),
        ]),
      );
    }
    if (cmd === "pty_cwds") {
      // Like the backend, only the requested sessions are answered.
      const ids = (args as { ids: number[] }).ids;
      return Object.fromEntries(Object.entries(liveCwds).filter(([id]) => ids.includes(Number(id))));
    }
  });

  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
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
  clearMocks();
  vi.useRealTimers();
});

describe("tabs", () => {
  test("starts with no tabs and an empty-state hint", async () => {
    await boot();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
    expect(callsOf("pty_spawn")).toHaveLength(0);
  });

  test("the terminal button sits after the tabs and opens a terminal tab", async () => {
    await boot();
    const bar = document.querySelector("#tabbar")!;
    expect(bar.lastElementChild?.id).toBe("tab-actions");
    expect(document.querySelector("#new-terminal svg")).not.toBeNull();
    await clickTerminal();
    expect(tabNames()).toEqual(["shell 1"]);
    expect(tabs()[0].querySelector(".icon")).toBeNull();
    expect(noTabs()).toBe(false);
    expect(spawns()[0]).toMatchObject({ kind: "terminal", command: null, claudeSession: null, cwd: null });
    expect(spawns()[0].tabId).toMatch(UUID);
  });

  test("a folder's Claude button opens a Claude tab in that folder", async () => {
    await boot();
    const button = document.querySelector<HTMLButtonElement>("#workspace-box .tree > ul .open-claude")!;
    expect(button.title).toBe("Open Claude in /home/me/work");
    expect(button.closest(".line")!.querySelector("button.row")!.textContent).toBe("work");
    await clickClaude();
    expect(tabNames()).toEqual(["Claude 1"]);
    expect(tabs()[0].querySelector(".icon svg")).not.toBeNull();
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "/home/me/work", fallbackToHome: false });
    expect(spawns()[0]).not.toHaveProperty("command");
    expect(spawns()[0].claudeSession).toMatch(UUID);
    // The folder itself stays closed.
    expect(callsOf("fs_list").map((c) => c.args.dir)).toEqual(["~"]);
  });

  test("the workspace folder itself has a Claude button too", async () => {
    await boot();
    const root = document.querySelector<HTMLButtonElement>("#workspace-box .tree .row.root + .open-claude")!;
    expect(root.title).toBe("Open Claude in ~");
    root.click();
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "~" });
  });

  test("each Claude tab gets its own session", async () => {
    await boot();
    await openClaudeTabs(2);
    const [a, b] = spawns().map((s) => s.claudeSession);
    expect(a).not.toBe(b);
  });

  test("a new terminal tab opens in the active tab's current directory", async () => {
    await boot();
    await clickClaude();
    liveCwds = { 1: "/home/me/src" };
    await clickTerminal();
    expect(spawns().map((s) => s.cwd)).toEqual(["/home/me/work", "/home/me/src"]);
  });

  test("closing the same tab twice kills it once and keeps the others", async () => {
    await boot();
    await openClaudeTabs(3);
    const x = tabs()[1].querySelector<HTMLButtonElement>(".x")!;
    x.click();
    x.click();
    expect(callsOf("pty_kill").map((c) => c.args.id)).toEqual([2]);
    expect(tabNames()).toEqual(["Claude 1", "Claude 3"]);
    expect(terms()[1].disposed).toBe(true);
  });

  test("middle-click closes a tab", async () => {
    await boot();
    await openClaudeTabs(2);
    tabs()[0].dispatchEvent(new MouseEvent("auxclick", { button: 1, bubbles: true }));
    expect(tabNames()).toEqual(["Claude 2"]);
  });

  test("closing the last tab keeps the window open and shows the hint", async () => {
    await boot();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
  });

  test("titles rename tabs until the user renames one", async () => {
    await boot();
    await clickClaude();
    terms()[0].handlers.title("✳ Fix the build");
    expect(tabNames()).toEqual(["✳ Fix the build"]);
    renameTab(0, "build");
    expect(tabNames()).toEqual(["build"]);
    terms()[0].handlers.title("✳ Something else");
    expect(tabNames()).toEqual(["build"]);
  });

  test("Escape cancels a rename", async () => {
    await boot();
    await clickClaude();
    renameTab(0, "discarded", "Escape");
    expect(tabNames()).toEqual(["Claude 1"]);
  });
});

describe("process lifecycle", () => {
  test("a failed spawn shows the error in an exited tab", async () => {
    spawnBehaviour = () => {
      throw { code: "cwd_missing", message: "not a directory: /nope" };
    };
    await boot();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    expect(terms()[0].output).toContain("could not start: not a directory: /nope");
  });

  test("shows the exit code and stops sending input", async () => {
    await boot();
    await clickClaude();
    spawns()[0].onExit.onmessage(3);
    expect(terms()[0].output).toContain("process exited with code 3");
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    terms()[0].handlers.data("hi\r");
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("Enter restarts an exited tab in place, with the same ids", async () => {
    await boot();
    await clickClaude();
    spawns()[0].onExit.onmessage(1);
    expect(terms()[0].output).toContain("press Enter to restart");
    terms()[0].handlers.data("x");
    expect(spawns()).toHaveLength(1);
    terms()[0].handlers.data("\r");
    terms()[0].handlers.data("\r");
    await flush();
    expect(spawns()).toHaveLength(2);
    const [first, again] = spawns();
    expect(again).toMatchObject({ tabId: first.tabId, claudeSession: first.claudeSession, fallbackToHome: true });
    expect(tabs()[0].classList.contains("exited")).toBe(false);
    expect(tabs()).toHaveLength(1);
    terms()[0].handlers.data("hi");
    expect(callsOf("pty_write").at(-1)?.args).toMatchObject({ id: 2 });
  });

  test("a tab that could not start can be restarted once the problem is gone", async () => {
    let fail = true;
    spawnBehaviour = (_args, id) => {
      if (fail) throw { code: "failed", message: "cannot check the transcript" };
      return id;
    };
    await boot();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    fail = false;
    terms()[0].handlers.data("\r");
    await flush();
    expect(tabs()[0].classList.contains("exited")).toBe(false);
  });

  test("a restarted tab's earlier hook watch stops; the new start has its own", async () => {
    await boot();
    await clickClaude();
    spawns()[0].onExit.onmessage(0);
    terms()[0].handlers.data("\r");
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
    await boot();
    await clickClaude();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
  });
});

describe("input", () => {
  test("text is sent as UTF-8 and binary input as raw bytes", async () => {
    await boot();
    await clickClaude();
    terms()[0].handlers.data("é");
    terms()[0].handlers.binary("\x80\x01");
    expect(callsOf("pty_write").map((c) => c.args.data)).toEqual([[0xc3, 0xa9], [0x80, 0x01]]);
  });

  test("resizes are forwarded to the PTY", async () => {
    await boot();
    await clickClaude();
    terms()[0].handlers.resize({ cols: 100, rows: 30 });
    expect(callsOf("pty_resize")[0].args).toMatchObject({ id: 1, cols: 100, rows: 30 });
  });

  test("links open through the opener plugin", async () => {
    await boot();
    await clickClaude();
    fakes.FakeWebLinksAddon.all[0].handler(new MouseEvent("click"), "https://tauri.app");
    await flush();
    expect(callsOf("plugin:opener|open_url")[0].args).toMatchObject({ url: "https://tauri.app" });
  });
});

describe("new terminal dialog", () => {
  test("asks only for a name and directory, with Start as the default button", async () => {
    await boot();
    expect(field("name")).not.toBeNull();
    expect(field("cwd")).not.toBeNull();
    expect(field("command")).toBeNull();
    expect(dialog().querySelector("button")?.value).toBe("ok");
  });

  test("Shift+click opens it and Start creates the terminal tab", async () => {
    await boot();
    await clickTerminal(true);
    expect(dialog().open).toBe(true);
    field("name")!.value = "agent";
    field("cwd")!.value = "~/src/raterm";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabNames()).toEqual(["agent"]);
    expect(spawns()[0]).toMatchObject({ kind: "terminal", cwd: "~/src/raterm" });
  });

  test("can open a Claude tab instead", async () => {
    await boot();
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
    await boot();
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
    await boot();
    expect(tabNames()).toEqual(["agent", "✳ Review", "logs", "shell"]);
    const [a, b, c, d] = spawns();
    expect(a).toMatchObject({ tabId: "t3", kind: "claude", cwd: "/home/me/src", claudeSession: SESSION });
    expect(b.kind).toBe("claude");
    expect(b.claudeSession).toMatch(UUID);
    expect(b.tabId).toBe("t4");
    expect(c).toMatchObject({ tabId: "t5", kind: "terminal", cwd: "/var/log", command: "tail -f syslog", claudeSession: null });
    // The backend types `claude --resume` at this shell's prompt.
    expect(d).toMatchObject({ tabId: "t6", kind: "terminal", cwd: "/home/me/app", command: null, claudeSession: SESSION });
    expect(tabs()[1].classList.contains("active")).toBe(true);
    expect(tabs()[2].querySelector(".icon")).toBeNull();
  });

  test("kills leftover sessions before restoring", async () => {
    savedLayout = layout;
    await boot();
    const order = calls.map((c) => c.cmd);
    expect(order.indexOf("pty_reset")).toBeLessThan(order.indexOf("pty_spawn"));
    expect(order.indexOf("pty_reset")).toBeLessThan(order.indexOf("layout_load"));
  });

  test("a restored tab keeps its id, so its session reports stay attached", async () => {
    savedLayout = layout;
    await boot();
    await win.closeRequested!();
    expect(lastSaved()?.tabs.map((t) => t.tabId)).toEqual(["t3", "t4", "t5", "t6"]);
  });

  test("restored user names survive titles; others follow them", async () => {
    savedLayout = layout;
    await boot();
    terms()[0].handlers.title("✳ x");
    terms()[1].handlers.title("✳ y");
    expect(tabNames()).toEqual(["agent", "✳ y", "logs", "shell"]);
  });

  test("a vanished directory falls back to the home directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "t8", kind: "claude", name: "old", renamed: true, cwd: "/gone", command: null, claudeSession: SESSION }],
    };
    missingDirs.add("/gone");
    await boot();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([["/gone", true]]);
    expect(spawns()[0].claudeSession).toBe(SESSION);
    expect(tabs()[0].classList.contains("exited")).toBe(false);
    expect(terms()[0].output).toContain("/gone no longer exists, so “old” started in the home directory.");
  });

  test("with nothing saved, starts with no tabs", async () => {
    savedLayout = { active: 0, tabs: [] };
    await boot();
    expect(tabs()).toHaveLength(0);
    expect(noTabs()).toBe(true);
  });
});

describe("saving tabs", () => {
  test("closing the window saves every tab with its live directory and session", async () => {
    await boot();
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
    await boot();
    spawns()[0].onExit.onmessage(0);
    terms()[0].handlers.data("\r");
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
      await boot();
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
      await boot();
      now += 61_000;
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
    });

    test("is kept when the check is unknown or fails", async () => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      savedLayout = restoredShell();
      await boot();
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
      await boot();
      spawns()[0].onExit.onmessage(0);
      await win.closeRequested!();
      expect(savedSession()).toBeNull();
    });

    test("without a conversation it is dropped, and a notice says why", async () => {
      resumeReport = { state: "noConversation" };
      savedLayout = restoredShell();
      await boot();
      expect(noticeText()).toContain("saved no conversation there");
      await win.closeRequested!();
      expect(savedSession()).toBeNull();
    });

    test("a resume that failed is kept for next time, and a notice says why", async () => {
      resumeReport = { state: "failed", message: "could not type the command" };
      savedLayout = restoredShell();
      await boot();
      expect(noticeText()).toContain("Claude could not be reopened in “shell” (could not type the command)");
      now += 61_000;
      claudeStates = { 1: { state: "unknown" } };
      await win.closeRequested!();
      expect(savedSession()).toBe(SESSION);
    });

    test("starting or quitting Claude is saved within seconds", async () => {
      await boot();
      await clickTerminal();
      openClaudes = { 1: SESSION };
      await vi.advanceTimersByTimeAsync(6_000);
      expect(savedSession()).toBe(SESSION);
      openClaudes = {};
      await vi.advanceTimersByTimeAsync(6_000);
      expect(savedSession()).toBeNull();
    });

    test("a claude that bypassed the wrapper is reported once it has run a while", async () => {
      await boot();
      await clickTerminal();
      claudeStates = { 1: { state: "untracked", pid: 77 } };
      await win.closeRequested!();
      expect(noticeText()).not.toContain("wrapper");
      now += 11_000;
      await win.closeRequested!();
      expect(noticeText()).toContain("Claude in “shell 1” did not start through raterm's claude wrapper");
      expect(savedSession()).toBeNull();
    });
  });

  test("tabs are still saved when the open Claude sessions cannot be checked", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await boot();
    await clickTerminal();
    failing.add("claude_open_sessions");
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0]).toMatchObject({ kind: "terminal", claudeSession: null });
    expect(error).toHaveBeenCalledWith("raterm: could not check for Claude sessions in terminal tabs", "claude_open_sessions failed");
    error.mockRestore();
  });

  test("closing the last tab saves an empty layout", async () => {
    await boot();
    await clickClaude();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await win.closeRequested!();
    expect(lastSaved()?.tabs).toEqual([]);
  });

  test("tab changes are saved shortly after they happen", async () => {
    await boot();
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
    await boot();
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
    await boot();
    await openClaudeTabs(2);
    await openSettings();
    expect(tabNames()).toEqual(["Claude 1", "Claude 2", "Settings"]);
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([false, false, true]);
    expect(pageShown()).toBe(true);
    expect([...document.querySelectorAll(".term.active")]).toHaveLength(0);
    expect(document.activeElement).toBe(page());
    expect(setting("skipPermissions").checked).toBe(false);

    await openSettings();
    expect(tabNames()).toEqual(["Claude 1", "Claude 2", "Settings"]);
    tabs()[0].click();
    expect(pageShown()).toBe(false);
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([true, false, false]);
    tabs()[2].click();
    expect(pageShown()).toBe(true);
    settingsTabX().click();
    expect(tabNames()).toEqual(["Claude 1", "Claude 2"]);
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
  });

  test("with no other tabs, the settings tab opens and closes back to the empty state", async () => {
    await boot();
    await openSettings();
    expect(tabNames()).toEqual(["Settings"]);
    expect(pageShown()).toBe(true);
    settingsTabX().click();
    expect(tabs()).toHaveLength(0);
    expect(pageShown()).toBe(false);
    expect(noTabs()).toBe(true);
  });

  test("the settings tab is not saved with the tabs", async () => {
    await boot();
    await openClaudeTabs(1);
    await openSettings();
    await win.closeRequested!();
    expect(lastSaved()!.tabs.map((t) => t.name)).toEqual(["Claude 1"]);
  });

  test("closing the terminal under a shown settings tab keeps settings shown", async () => {
    await boot();
    await openClaudeTabs(2);
    await openSettings();
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(pageShown()).toBe(true);
    expect(document.querySelectorAll(".term.active")).toHaveLength(0);
    settingsTabX().click();
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
  });

  test("the page shows the stored value each time it opens, and saves changes", async () => {
    await boot();
    await openSettings();
    expect(setting("skipPermissions").checked).toBe(false);
    await change("skipPermissions", true);
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ skipPermissions: true, workspace: null });
    expect(pageError().hidden).toBe(true);
    settingsTabX().click();
    // Changed outside the page (another window, or a tab writing the database).
    storedSettings = { ...DEFAULTS };
    await openSettings();
    expect(setting("skipPermissions").checked).toBe(false);
  });

  test("the form stays disabled until the stored settings are read", async () => {
    storedSettings = { ...DEFAULTS, skipPermissions: true };
    let release!: () => void;
    settingsLoadGate = new Promise((r) => (release = r));
    await boot();
    await openSettings();
    expect(disabled()).toBe(true);
    release();
    await flush();
    await flush();
    expect(disabled()).toBe(false);
    expect(setting("skipPermissions").checked).toBe(true);
  });

  test("a failed save is shown and the checkbox goes back to the stored value", async () => {
    await boot();
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
    await boot();
    await openSettings();
    await change("skipPermissions", true);
    await change("skipPermissions", false);
    expect(callsOf("settings_save").map((c) => (c.args.settings as Settings).skipPermissions)).toEqual([true, false]);
    expect(storedSettings.skipPermissions).toBe(false);
  });

  test("unreadable settings are shown and cannot be changed", async () => {
    failing.add("settings_load");
    await boot();
    await openSettings();
    expect(disabled()).toBe(true);
    expect(pageError().textContent).toContain("Claude tabs start with the defaults");
  });

  test("Claude tabs started with skipped permissions are marked", async () => {
    storedSettings = { ...DEFAULTS, skipPermissions: true };
    await boot();
    await openClaudeTabs(1);
    expect(tabs()[0].querySelector(".badge")).not.toBeNull();
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    storedSettings = { ...DEFAULTS };
    terms()[0].handlers.title?.("busy");
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    // The restart picks up the current setting.
    spawns()[0].onExit.onmessage(0);
    terms()[0].handlers.data("\r");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabs()[0].querySelector(".badge")).toBeNull();
  });

  test("without a database the page says settings are not kept", async () => {
    storeReport = { store: { state: "notSaving", reason: "no data dir" }, hookProblem: null };
    await boot();
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
    await boot();
    await settle();
    expect(rows()).toEqual(["~", "notes"]);
  });

  test("shows the stored folder and does not follow the tabs", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await boot();
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
    await boot();
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
    await boot();
    await settle();
    row("src").click();
    await settle();
    fsTree["/p/src"].push({ name: "new.ts", dir: false });
    button(".refresh").click();
    await settle();
    expect(rows()).toEqual(["p", "src", "main.ts", "new.ts", "my notes.txt"]);
  });

  test("the gear opens a workspace settings tab, where the folder is chosen", async () => {
    await boot();
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
    await boot();
    button(".configure").click();
    await settle();
    picker.picked = "/gone";
    folderField().click();
    await settle();
    expect(pageError().textContent).toContain("/gone is not a folder");
    expect(rows()).toEqual(["~", "notes"]);

    vi.resetModules();
    storedSettings = { ...DEFAULTS, workspace: "/vanished" };
    await boot();
    await settle();
    expect(treeText()).toContain("cannot read /vanished");
  });

  test("with unreadable settings the tree shows home and the folder cannot be changed", async () => {
    failing.add("settings_load");
    await boot();
    await settle();
    expect(rows()).toEqual(["~", "notes"]);
    button(".configure").click();
    await settle();
    expect(document.querySelector<HTMLFieldSetElement>("#workspace-settings fieldset")!.disabled).toBe(true);
  });

  test("both pages can be open as their own tabs next to the terminals", async () => {
    await boot();
    await openClaudeTabs(1);
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    button(".configure").click();
    await settle();
    expect(tabNames()).toEqual(["Claude 1", "Settings", "Workspace settings"]);
    const shown = () => [...document.querySelectorAll(".page.active")].map((p) => p.id);
    expect(shown()).toEqual(["workspace-settings"]);
    tabs()[1].click();
    expect(shown()).toEqual(["settings"]);
    tabs()[1].querySelector<HTMLButtonElement>(".x")!.click();
    expect(tabNames()).toEqual(["Claude 1", "Workspace settings"]);
    expect(shown()).toEqual([]);
    expect(document.querySelectorAll(".term")[0].classList.contains("active")).toBe(true);
    tabs()[1].click();
    expect(shown()).toEqual(["workspace-settings"]);
  });

  test("file names that could act as keys are not typed, and say why", async () => {
    fsTree["/p"].push({ name: "notes\x15curl x|sh\r.txt", dir: false }, { name: "a\\b", dir: false });
    storedSettings = { ...DEFAULTS, workspace: "/p" };
    await boot();
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
    await boot();
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
    await boot();
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
    await boot();
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
    await boot();
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
    await boot();
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
    await boot();
    document.querySelector<HTMLButtonElement>("#open-settings")!.click();
    await settle();
    const skip = document.querySelector<HTMLInputElement>("#settings input[name=skipPermissions]")!;
    skip.checked = true;
    skip.dispatchEvent(new Event("change", { bubbles: true }));
    await settle();
    expect(storedSettings).toEqual({ skipPermissions: true, workspace: "/p" });
    button(".configure").click();
    await settle();
    pageButton(".home-workspace").click();
    await settle();
    expect(storedSettings).toEqual({ skipPermissions: true, workspace: null });
  });

  test("a workspace folder that has gone does not block other settings", async () => {
    storedSettings = { ...DEFAULTS, workspace: "/vanished" };
    await boot();
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
    await boot();
    await settle();
    expect(noticeText()).toContain("the Workspace box shows your home directory");
    button(".configure").click();
    await settle();
    expect(pageError().textContent).toContain("its folder cannot be changed");
  });

  test("every icon in the page is drawn", async () => {
    await boot();
    const icons = [...document.querySelectorAll("[data-icon]")];
    expect(icons.length).toBeGreaterThan(5);
    for (const el of icons) expect(el.querySelector("svg"), (el as HTMLElement).dataset.icon).not.toBeNull();
  });

  test("the toolbox collapses and remembers it", async () => {
    await boot();
    const box = () => document.querySelector("#workspace-box")!;
    button(".toggle").click();
    expect(box().classList.contains("collapsed")).toBe(true);
    // A fresh app instance on a fresh page.
    vi.resetModules();
    await boot();
    expect(box().classList.contains("collapsed")).toBe(true);
    vi.resetModules();
    localStorage.clear();
    await boot();
    expect(box().classList.contains("collapsed")).toBe(false);
  });
});

describe("switching and layout", () => {
  test("clicking a tab makes it and its terminal active", async () => {
    await boot();
    await openClaudeTabs(2);
    tabs()[0].click();
    expect(tabs().map((t) => t.classList.contains("active"))).toEqual([true, false]);
    const panes = [...document.querySelectorAll(".term")];
    expect(panes.map((p) => p.classList.contains("active"))).toEqual([true, false]);
  });

  test("the sidebar toggle collapses it and remembers the choice", async () => {
    await boot();
    const collapsed = () => document.body.classList.contains("sidebar-collapsed");
    expect(collapsed()).toBe(false);
    document.querySelector<HTMLButtonElement>("#toggle-sidebar")!.click();
    expect(collapsed()).toBe(true);

    // A fresh app instance on a fresh page.
    vi.resetModules();
    document.body.className = "";
    await boot();
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
    await boot();
    expect(tabs()[1].classList.contains("active")).toBe(true);
  });
});

describe("new tab directory", () => {
  test("falls back to the active tab's saved directory when its live one is unknown", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: null }],
    };
    await boot();
    await clickTerminal();
    expect(spawns()[1].cwd).toBe("/srv/app");
  });

  test("still opens a tab when the directory lookup fails", async () => {
    await boot();
    await clickTerminal();
    failing.add("pty_cwds");
    await clickTerminal();
    expect(tabs()).toHaveLength(2);
    expect(spawns()[1].cwd).toBeNull();
  });

  test("the dialog uses the current tab's directory when left empty", async () => {
    await boot();
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
    await boot();
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
    await boot();
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
    await boot();
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
  });

  test("a failed directory lookup still saves, with each tab's original directory", async () => {
    savedLayout = {
      active: 0,
      tabs: [{ tabId: "a", kind: "claude", name: "a", renamed: true, cwd: "/srv/app", command: null, claudeSession: null }],
    };
    await boot();
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
    await boot();
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
    await boot();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    await win.closeRequested!();
    expect(lastSaved()?.tabs[0].cwd).toBe("/gone");
  });

  test("a failed save shows a notice that clears after the next good save", async () => {
    await boot();
    await clickClaude();
    failing.add("layout_save");
    await win.closeRequested!();
    expect(noticeText()).toContain("Could not save tabs: layout_save failed");
    failing.delete("layout_save");
    await win.closeRequested!();
    expect(noticeVisible()).toBe(false);
  });

  test("the backend's store status is shown, and only dismissible notices can be dismissed", async () => {
    storeReport.store = { state: "notSaving", reason: "the database was written by a newer raterm" };
    await boot();
    expect(noticeText()).toContain("Tabs are not being saved: the database was written by a newer raterm.");
    expect(dismissButton().hidden).toBe(true);
  });

  test("a recovery notice can be dismissed and stays dismissed", async () => {
    storeReport.store = { state: "recovered", notice: "Saved tabs could not be read, so raterm started fresh." };
    await boot();
    expect(noticeVisible()).toBe(true);
    dismissButton().click();
    expect(noticeVisible()).toBe(false);
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
  });

  test("a dismissed hook failure stays hidden until a new one is logged", async () => {
    storeReport.hookProblem = { at: 2000, message: "first failure", log: LOG };
    await boot();
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
    await boot();
    await clickClaude();
    failing.add("layout_save");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await expect(win.closeRequested!()).resolves.not.toThrow();
    expect(error).toHaveBeenCalledWith("raterm: could not save tabs", "layout_save failed");
    expect(win.prevented).toBe(true);
    expect(noticeText()).toContain("Close the window again to quit without saving.");
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
    error.mockRestore();
  });

  test("a close after a good save is not cancelled", async () => {
    await boot();
    await clickClaude();
    await win.closeRequested!();
    expect(win.prevented).toBe(false);
  });

  test("a failed save is tried again until it works", async () => {
    await boot();
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
    await boot();
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
    await boot();
    await clickClaude();
    terms()[0].handlers.data("early");
    terms()[0].handlers.resize({ cols: 90, rows: 20 });
    expect(callsOf("pty_write")).toHaveLength(0);
    expect(callsOf("pty_resize")).toHaveLength(0);
  });
});

describe("edge cases", () => {
  test("an unchanged layout is not written again", async () => {
    await boot();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(1000);
    const saves = callsOf("layout_save").length;
    terms()[0].handlers.title("Claude 1");
    await vi.advanceTimersByTimeAsync(1000);
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(saves);
    renameTab(0, "renamed");
    await vi.advanceTimersByTimeAsync(1000);
    expect(callsOf("layout_save")).toHaveLength(saves + 1);
  });

  test("switching tabs saves which one is active", async () => {
    await boot();
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
    await boot();
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
    await boot();
    expect(tabNames()).toEqual(["b"]);
    expect(document.querySelectorAll(".term")).toHaveLength(1);
    expect(noticeText()).toContain("A tab could not be opened: no canvas.");
    expect(noticeText()).toContain("1 saved tab(s) could not be reopened");
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
    error.mockRestore();
  });

  test("a restore that fails outright switches saving off", async () => {
    savedLayout = { active: 0, tabs: null } as unknown as Layout;
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await boot();
    expect(noticeText()).toContain("Saved tabs could not be restored");
    await clickClaude();
    await win.closeRequested!();
    expect(callsOf("layout_save")).toHaveLength(0);
    error.mockRestore();
  });

  test("a failed session reset is shown", async () => {
    failing.add("pty_reset");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await boot();
    expect(noticeText()).toContain("Leftover sessions could not be stopped");
    error.mockRestore();
  });

  test("hook failures logged by the backend are picked up by the status check", async () => {
    await boot();
    expect(noticeVisible()).toBe(false);
    storeReport.hookProblem = { at: 2000, message: "schema 2 in raterm.db, this binary knows 1", log: LOG };
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).toContain("Claude session tracking failed (schema 2 in raterm.db");
    expect(noticeText()).toContain(`see ${LOG}.`);
  });

  test("a Claude tab whose hook never reports gets a warning", async () => {
    await boot();
    await clickClaude();
    await clickClaude();
    reported.add(spawns()[1].tabId);
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("Claude in “Claude 1” has not reported its session");
    expect(noticeText()).not.toContain("Claude 2");
    expect(callsOf("claude_reported")[0].args).toMatchObject({ since: 1000 });
  });

  test("the warning clears once the hook reports, or when the tab closes", async () => {
    await boot();
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
    await boot();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).toContain("has not reported");
    dismissButton().click();
    terms()[0].handlers.title("✳ Fix the build");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
    terms()[0].handlers.title("✳ Something else");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeVisible()).toBe(false);
  });

  test("a failing report check neither warns nor gives up", async () => {
    await boot();
    await clickClaude();
    failing.add("claude_reported");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).not.toContain("has not reported");
    expect(error).toHaveBeenCalledWith("raterm: could not check the session report", "claude_reported failed");
    failing.delete("claude_reported");
    await vi.advanceTimersByTimeAsync(31_000);
    expect(noticeText()).toContain("has not reported");
    error.mockRestore();
  });

  test("the warning goes away when the tab's process exits", async () => {
    await boot();
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
    await boot();
    await clickClaude();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(noticeText()).not.toContain("has not reported");
    expect(callsOf("claude_reported")).toHaveLength(0);
  });

  test("starting in the home directory is also shown as a notice", async () => {
    await boot();
    await clickTerminal();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickTerminal();
    expect(noticeText()).toContain("/gone no longer exists, so “shell 2” started in the home directory.");
  });

  test("a new tab from a vanished active directory starts in the home directory", async () => {
    await boot();
    await clickTerminal();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickTerminal();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([[null, true], ["/gone", true]]);
    expect(tabs()[1].classList.contains("exited")).toBe(false);
  });

  test("a directory typed into the dialog does not fall back silently", async () => {
    missingDirs.add("/typo");
    await boot();
    await clickTerminal(true);
    field("cwd")!.value = "/typo";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([["/typo", false]]);
    expect(tabs()[0].classList.contains("exited")).toBe(true);
  });

  test("a claude wrapper that could not be installed is reported", async () => {
    storeReport.wrapperProblem = "cannot write the claude wrapper in /d/bin: Read-only file system";
    await boot();
    expect(noticeText()).toContain(
      "Claude sessions started in terminal tabs will not be reopened: cannot write the claude wrapper in /d/bin",
    );
  });

  test("restores the layout fixture the Rust tests also parse", async () => {
    savedLayout = JSON.parse(sharedLayout);
    await boot();
    expect(tabNames()).toEqual(["agent", "logs", "shell"]);
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
