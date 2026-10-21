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
let failing: Set<string>;
let storeReport: {
  store: { state: "saving" } | { state: "recovered"; notice: string } | { state: "notSaving"; reason: string };
  hookProblem: { at: number; message: string; log: string } | null;
};
let reported: Set<string>;
let missingDirs: Set<string>;
type Settings = Record<string, unknown>;
let storedSettings: Settings;
/** settings_load waits for this, so a test can hold the load back. */
let settingsLoadGate: Promise<void>;
const DEFAULTS: Settings = { skipPermissions: false };

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

async function clickClaude(shiftKey = false) {
  const button = document.querySelector<HTMLButtonElement>("#new-claude")!;
  button.dispatchEvent(new MouseEvent("click", { bubbles: true, shiftKey }));
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
  failing = new Set();
  storeReport = { store: { state: "saving" }, hookProblem: null };
  reported = new Set();
  missingDirs = new Set();
  storedSettings = { ...DEFAULTS };
  settingsLoadGate = Promise.resolve();
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
      return Promise.resolve(id).then((id) => ({ id, fellBackToHome: missing, tracked, skipsPermissions, startedAt: 1000 }));
    }
    if (cmd === "store_status") return storeReport;
    if (cmd === "claude_reported") return reported.has((args as { tabId: string }).tabId);
    if (cmd === "layout_load") return savedLayout;
    if (cmd === "settings_load") return settingsLoadGate.then(() => storedSettings);
    if (cmd === "settings_save") {
      // Like the backend: stores and returns what it stored.
      storedSettings = (args as { settings: Settings }).settings;
      return storedSettings;
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

  test("the Claude button sits after the tabs and opens a Claude tab", async () => {
    await boot();
    const bar = document.querySelector("#tabbar")!;
    expect(bar.lastElementChild?.id).toBe("tab-actions");
    await clickClaude();
    expect(tabNames()).toEqual(["Claude 1"]);
    expect(tabs()[0].querySelector(".icon svg")).not.toBeNull();
    expect(noTabs()).toBe(false);
    expect(spawns()[0].kind).toBe("claude");
    expect(spawns()[0]).not.toHaveProperty("command");
    expect(spawns()[0].tabId).toMatch(UUID);
    expect(spawns()[0].claudeSession).toMatch(UUID);
  });

  test("each Claude tab gets its own session", async () => {
    await boot();
    await openClaudeTabs(2);
    const [a, b] = spawns().map((s) => s.claudeSession);
    expect(a).not.toBe(b);
  });

  test("a new Claude tab opens in the active tab's current directory", async () => {
    await boot();
    await clickClaude();
    liveCwds = { 1: "/home/me/src" };
    await clickClaude();
    expect(spawns().map((s) => s.cwd)).toEqual([null, "/home/me/src"]);
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

describe("new Claude session dialog", () => {
  test("asks only for a name and directory, with Start as the default button", async () => {
    await boot();
    expect(field("name")).not.toBeNull();
    expect(field("cwd")).not.toBeNull();
    expect(field("command")).toBeNull();
    expect(dialog().querySelector("button")?.value).toBe("ok");
  });

  test("Shift+click opens it and Start creates the Claude tab", async () => {
    await boot();
    await clickClaude(true);
    expect(dialog().open).toBe(true);
    field("name")!.value = "agent";
    field("cwd")!.value = "~/src/raterm";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(tabNames()).toEqual(["agent"]);
    expect(spawns()[0]).toMatchObject({ kind: "claude", cwd: "~/src/raterm" });
  });

  test("Escape after an earlier Start does not spawn", async () => {
    await boot();
    await clickClaude(true);
    dialog().close("ok");
    await flush();
    await clickClaude(true);
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
    ],
  };

  test("reopens each tab with its kind, directory, command and session", async () => {
    savedLayout = layout;
    await boot();
    expect(tabNames()).toEqual(["agent", "✳ Review", "logs"]);
    const [a, b, c] = spawns();
    expect(a).toMatchObject({ tabId: "t3", kind: "claude", cwd: "/home/me/src", claudeSession: SESSION });
    expect(b.kind).toBe("claude");
    expect(b.claudeSession).toMatch(UUID);
    expect(b.tabId).toBe("t4");
    expect(c).toMatchObject({ tabId: "t5", kind: "terminal", cwd: "/var/log", command: "tail -f syslog" });
    expect(c).not.toHaveProperty("claudeSession");
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
    expect(lastSaved()?.tabs.map((t) => t.tabId)).toEqual(["t3", "t4", "t5"]);
  });

  test("restored user names survive titles; others follow them", async () => {
    savedLayout = layout;
    await boot();
    terms()[0].handlers.title("✳ x");
    terms()[1].handlers.title("✳ y");
    expect(tabNames()).toEqual(["agent", "✳ y", "logs"]);
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
    await clickClaude(true);
    field("name")!.value = "agent";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    liveCwds = { 1: "/home/me/src", 2: "/home/me" };
    await win.closeRequested!();
    const [first, second] = spawns().map((s) => s.claudeSession);
    const [firstTab, secondTab] = spawns().map((s) => s.tabId);
    expect(firstTab).not.toBe(secondTab);
    expect(lastSaved()).toEqual({
      active: 1,
      tabs: [
        { tabId: firstTab, kind: "claude", name: "Claude 1", renamed: false, cwd: "/home/me/src", claudeSession: first },
        { tabId: secondTab, kind: "claude", name: "agent", renamed: true, cwd: "/home/me", claudeSession: second },
      ],
    });
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
    expect(callsOf("settings_save").at(-1)!.args.settings).toEqual({ skipPermissions: true });
    expect(pageError().hidden).toBe(true);
    settingsTabX().click();
    // Changed outside the page (another window, or a tab writing the database).
    storedSettings = { skipPermissions: false };
    await openSettings();
    expect(setting("skipPermissions").checked).toBe(false);
  });

  test("the form stays disabled until the stored settings are read", async () => {
    storedSettings = { skipPermissions: true };
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
    storedSettings = { skipPermissions: true };
    await boot();
    await openClaudeTabs(1);
    expect(tabs()[0].querySelector(".badge")).not.toBeNull();
    expect(tabs()[0].title).toContain("--dangerously-skip-permissions");
    storedSettings = { skipPermissions: false };
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
    await clickClaude();
    expect(spawns()[1].cwd).toBe("/srv/app");
  });

  test("still opens a tab when the directory lookup fails", async () => {
    await boot();
    await clickClaude();
    failing.add("pty_cwds");
    await clickClaude();
    expect(tabs()).toHaveLength(2);
    expect(spawns()[1].cwd).toBeNull();
  });

  test("the dialog uses the current tab's directory when left empty", async () => {
    await boot();
    await clickClaude();
    liveCwds = { 1: "/home/me/x" };
    await clickClaude(true);
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
    await clickClaude();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickClaude();
    expect(noticeText()).toContain("/gone no longer exists, so “Claude 2” started in the home directory.");
  });

  test("a new tab from a vanished active directory starts in the home directory", async () => {
    await boot();
    await clickClaude();
    liveCwds = { 1: "/gone" };
    missingDirs.add("/gone");
    await clickClaude();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([[null, true], ["/gone", true]]);
    expect(tabs()[1].classList.contains("exited")).toBe(false);
  });

  test("a directory typed into the dialog does not fall back silently", async () => {
    missingDirs.add("/typo");
    await boot();
    await clickClaude(true);
    field("cwd")!.value = "/typo";
    dialog().close("ok");
    for (let i = 0; i < 3; i++) await flush();
    expect(spawns().map((s) => [s.cwd, s.fallbackToHome])).toEqual([["/typo", false]]);
    expect(tabs()[0].classList.contains("exited")).toBe(true);
  });

  test("restores the layout fixture the Rust tests also parse", async () => {
    savedLayout = JSON.parse(sharedLayout);
    await boot();
    expect(tabNames()).toEqual(["agent", "logs"]);
    expect(spawns()[0]).toMatchObject({ kind: "claude", tabId: "t1", claudeSession: SESSION, cwd: "/home/me/src" });
    expect(spawns()[1]).toMatchObject({ kind: "terminal", tabId: "t2", command: "tail -f syslog" });
    await win.closeRequested!();
    expect(lastSaved()).toEqual(JSON.parse(sharedLayout));
  });

  test("the window may be destroyed after the close-time save", () => {
    expect(JSON.parse(capabilities).permissions).toContain("core:window:allow-destroy");
  });
});
