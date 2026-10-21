import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { Channel } from "@tauri-apps/api/core";
import indexHtml from "../index.html?raw";

// xterm needs a real canvas and layout, so the tests drive a stand-in that
// records writes and exposes the handlers main.ts registers.
const fakes = vi.hoisted(() => {
  type Handler = (...args: any[]) => void;
  class FakeTerminal {
    static all: FakeTerminal[] = [];
    cols = 80;
    rows = 24;
    written: string[] = [];
    handlers: Record<string, Handler> = {};
    disposed = false;
    constructor() {
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
    open() {}
    focus() {}
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

vi.mock("@xterm/xterm/css/xterm.css", () => ({}));
vi.mock("@xterm/xterm", () => ({ Terminal: fakes.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: fakes.FakeWebLinksAddon }));

type Call = { cmd: string; args: Record<string, unknown> };
type SpawnArgs = { onExit: Channel<number>; command: string | null };

let calls: Call[];
let spawnBehaviour: (args: SpawnArgs, id: number) => number;

const terms = () => fakes.FakeTerminal.all;
const tabs = () => [...document.querySelectorAll<HTMLElement>("#tabs .tab")];
const tabNames = () => tabs().map((t) => t.querySelector(".name")?.textContent);
const callsOf = (cmd: string) => calls.filter((c) => c.cmd === cmd);
const dialog = () => document.querySelector<HTMLDialogElement>("#spawn-dialog")!;
const plusButton = () => document.querySelector<HTMLButtonElement>("#new-session")!;
const flush = () => new Promise((r) => setTimeout(r, 0));

async function boot() {
  document.body.innerHTML = new DOMParser().parseFromString(indexHtml, "text/html").body.innerHTML;
  await import("./main");
  await flush();
}

async function clickPlus(shiftKey = false) {
  plusButton().dispatchEvent(new MouseEvent("click", { bubbles: true, shiftKey }));
  await flush();
}

function key(el: HTMLElement, key: string) {
  el.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
}

beforeEach(() => {
  vi.resetModules();
  fakes.FakeTerminal.all = [];
  fakes.FakeWebLinksAddon.all = [];
  calls = [];
  let nextId = 0;
  spawnBehaviour = (_args, id) => id;
  mockWindows("main");
  mockIPC((cmd, args) => {
    calls.push({ cmd, args: (args ?? {}) as Record<string, unknown> });
    if (cmd === "pty_spawn") return spawnBehaviour(args as unknown as SpawnArgs, ++nextId);
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

afterEach(() => clearMocks());

describe("tabs", () => {
  test("starts one login-shell tab on launch", async () => {
    await boot();
    expect(tabNames()).toEqual(["shell 1"]);
    expect(callsOf("pty_spawn")).toHaveLength(1);
    expect(callsOf("pty_spawn")[0].args.command).toBeNull();
  });

  test("+ adds a tab and makes it active", async () => {
    await boot();
    await clickPlus();
    expect(tabNames()).toEqual(["shell 1", "shell 2"]);
    expect(tabs()[1].classList.contains("active")).toBe(true);
    expect(tabs()[0].classList.contains("active")).toBe(false);
  });

  test("closing the same tab twice kills it once and keeps the others", async () => {
    await boot();
    await clickPlus();
    await clickPlus();
    const x = tabs()[1].querySelector<HTMLButtonElement>(".x")!;
    x.click();
    x.click();
    expect(callsOf("pty_kill").map((c) => c.args.id)).toEqual([2]);
    expect(tabNames()).toEqual(["shell 1", "shell 3"]);
    expect(terms()[1].disposed).toBe(true);
  });

  test("middle-click closes a tab", async () => {
    await boot();
    await clickPlus();
    tabs()[0].dispatchEvent(new MouseEvent("auxclick", { button: 1, bubbles: true }));
    expect(tabNames()).toEqual(["shell 2"]);
  });

  test("closing the last tab closes the window", async () => {
    await boot();
    tabs()[0].querySelector<HTMLButtonElement>(".x")!.click();
    await flush();
    expect(callsOf("plugin:window|close")).toHaveLength(1);
  });

  test("shell titles rename tabs until the user renames one", async () => {
    await boot();
    terms()[0].handlers.title("vim");
    expect(tabNames()).toEqual(["vim"]);

    tabs()[0].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    const input = tabs()[0].querySelector("input")!;
    input.value = "notes";
    key(input, "Enter");
    expect(tabNames()).toEqual(["notes"]);

    terms()[0].handlers.title("bash");
    expect(tabNames()).toEqual(["notes"]);
  });

  test("Escape cancels a rename", async () => {
    await boot();
    tabs()[0].dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    const input = tabs()[0].querySelector("input")!;
    input.value = "discarded";
    key(input, "Escape");
    input.dispatchEvent(new FocusEvent("blur"));
    expect(tabNames()).toEqual(["shell 1"]);
  });
});

describe("process lifecycle", () => {
  test("a failed spawn shows the error in an exited tab", async () => {
    spawnBehaviour = () => {
      throw "not a directory: /nope";
    };
    await boot();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    expect(terms()[0].output).toContain("could not start: not a directory: /nope");
  });

  test("shows the exit code and stops sending input", async () => {
    await boot();
    const { onExit } = callsOf("pty_spawn")[0].args as unknown as SpawnArgs;
    onExit.onmessage(3);
    expect(terms()[0].output).toContain("process exited with code 3");
    expect(tabs()[0].classList.contains("exited")).toBe(true);

    terms()[0].handlers.data("ls\r");
    expect(callsOf("pty_write")).toHaveLength(0);
  });

  test("an exit that arrives before pty_spawn resolves is not lost", async () => {
    spawnBehaviour = (args, id) => {
      args.onExit.onmessage(0);
      return id;
    };
    await boot();
    expect(tabs()[0].classList.contains("exited")).toBe(true);
    expect(terms()[0].output).toContain("process exited");
  });
});

describe("input", () => {
  test("text is sent as UTF-8 and binary input as raw bytes", async () => {
    await boot();
    terms()[0].handlers.data("é");
    terms()[0].handlers.binary("\x80\x01");
    expect(callsOf("pty_write").map((c) => c.args.data)).toEqual([[0xc3, 0xa9], [0x80, 0x01]]);
  });

  test("resizes are forwarded to the PTY", async () => {
    await boot();
    terms()[0].handlers.resize({ cols: 100, rows: 30 });
    expect(callsOf("pty_resize")[0].args).toMatchObject({ id: 1, cols: 100, rows: 30 });
  });

  test("links open through the opener plugin", async () => {
    await boot();
    fakes.FakeWebLinksAddon.all[0].handler(new MouseEvent("click"), "https://tauri.app");
    await flush();
    expect(callsOf("plugin:opener|open_url")[0].args).toMatchObject({ url: "https://tauri.app" });
  });
});

describe("spawn dialog", () => {
  test("Start is the form's first submit button, so Enter starts the session", async () => {
    await boot();
    expect(dialog().querySelector("button")?.value).toBe("ok");
  });

  test("Start spawns with the entered options", async () => {
    await boot();
    await clickPlus(true);
    expect(dialog().open).toBe(true);
    const form = dialog().querySelector("form")!;
    (form.elements.namedItem("name") as HTMLInputElement).value = "agent";
    (form.elements.namedItem("command") as HTMLInputElement).value = "claude";
    dialog().close("ok");
    await flush();
    expect(tabNames()).toEqual(["shell 1", "agent"]);
    expect(callsOf("pty_spawn")[1].args.command).toBe("claude");
  });

  test("Escape after an earlier Start does not spawn", async () => {
    await boot();
    await clickPlus(true);
    dialog().close("ok");
    await flush();
    await clickPlus(true);
    dialog().close();
    await flush();
    expect(callsOf("pty_spawn")).toHaveLength(2);
  });
});
