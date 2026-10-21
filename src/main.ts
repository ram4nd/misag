import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

interface Session {
  /** Backend PTY id; null until pty_spawn resolves, and forever if it failed. */
  id: number | null;
  name: string;
  /** True once the user renamed the tab, so shell titles stop overwriting it. */
  renamed: boolean;
  term: Terminal;
  fit: FitAddon;
  el: HTMLDivElement;
  exited: boolean;
  closing: boolean;
}

interface SpawnOptions {
  name?: string;
  cwd?: string;
  command?: string;
}

// xfce4-terminal defaults.
const THEME = {
  background: "#000000",
  foreground: "#ffffff",
  cursor: "#ffffff",
  selectionBackground: "#555555",
  black: "#000000", red: "#aa0000", green: "#00aa00", yellow: "#aa5500",
  blue: "#0000aa", magenta: "#aa00aa", cyan: "#00aaaa", white: "#aaaaaa",
  brightBlack: "#555555", brightRed: "#ff5555", brightGreen: "#55ff55",
  brightYellow: "#ffff55", brightBlue: "#5555ff", brightMagenta: "#ff55ff",
  brightCyan: "#55ffff", brightWhite: "#ffffff",
};

function $<T extends HTMLElement>(sel: string): T {
  const el = document.querySelector<T>(sel);
  if (!el) throw new Error(`raterm: missing element ${sel}`);
  return el;
}
const tabsEl = $<HTMLDivElement>("#tabs");
const termsEl = $<HTMLElement>("#terminals");
const spawnDialog = $<HTMLDialogElement>("#spawn-dialog");

const sessions: Session[] = [];
let active: Session | null = null;
let counter = 0;
const encoder = new TextEncoder();

// ---------- sessions ----------

async function spawn(opts: SpawnOptions = {}) {
  const el = document.createElement("div");
  el.className = "term";
  termsEl.append(el);

  const term = new Terminal({
    fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
    fontSize: 14,
    cursorBlink: true,
    scrollback: 10000,
    theme: THEME,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(
    new WebLinksAddon((_e, uri) => {
      openUrl(uri).catch((err) => console.error("raterm: could not open link", uri, err));
    }),
  );
  term.open(el);

  const s: Session = {
    id: null, term, fit, el, exited: false, closing: false,
    renamed: Boolean(opts.name),
    name: opts.name || opts.command?.split(" ")[0] || `shell ${++counter}`,
  };
  sessions.push(s);
  renderSessions();
  activate(s);
  // The pane must be visible and sized before the PTY is opened with its dimensions.
  fit.fit();

  const onData = new Channel<ArrayBuffer>();
  onData.onmessage = (buf) => term.write(new Uint8Array(buf));
  // Attached before pty_spawn runs, so an immediate exit is not missed.
  const onExit = new Channel<number>();
  onExit.onmessage = (code) =>
    markExited(s, code === 0 ? "process exited" : `process exited with code ${code}`);

  term.onData((data) => write(s, encoder.encode(data)));
  term.onBinary((data) => write(s, Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff)));
  term.onResize(({ cols, rows }) => {
    if (s.id === null || s.exited) return;
    invoke("pty_resize", { id: s.id, cols, rows }).catch((err) =>
      console.error("raterm: resize failed", err),
    );
  });
  term.onTitleChange((title) => {
    if (title && !s.renamed) {
      s.name = title;
      renderSessions();
    }
  });

  try {
    s.id = await invoke<number>("pty_spawn", {
      cols: term.cols,
      rows: term.rows,
      cwd: opts.cwd || null,
      command: opts.command || null,
      onData,
      onExit,
    });
  } catch (err) {
    markExited(s, `could not start: ${err}`, true);
  }
}

function startSession(opts?: SpawnOptions) {
  spawn(opts).catch((err) => console.error("raterm: could not create session", err));
}

function write(s: Session, data: Uint8Array) {
  if (s.id === null || s.exited) return;
  invoke("pty_write", { id: s.id, data: Array.from(data) }).catch((err) =>
    console.error("raterm: write failed", err),
  );
}

function markExited(s: Session, message: string, error = false) {
  if (s.closing) return;
  s.exited = true;
  s.term.write(`\r\n\x1b[${error ? "31" : "2"}m[${message}]\x1b[0m\r\n`);
  renderSessions();
}

function activate(s: Session) {
  active = s;
  // Toggle classes in place rather than re-rendering, so a click followed by a
  // double-click (rename) lands on the same tab element.
  sessions.forEach((o, i) => {
    o.el.classList.toggle("active", o === s);
    tabsEl.children[i]?.classList.toggle("active", o === s);
  });
  requestAnimationFrame(() => {
    s.fit.fit();
    s.term.focus();
  });
}

function close(s: Session) {
  if (s.closing) return;
  s.closing = true;
  const i = sessions.indexOf(s);
  sessions.splice(i, 1);
  // A missing session (already exited) is a no-op on the backend.
  if (s.id !== null) {
    invoke("pty_kill", { id: s.id }).catch((err) => console.error("raterm: kill failed", err));
  }
  s.term.dispose();
  s.el.remove();

  // Like Chrome, closing the last tab closes the window.
  if (!sessions.length) {
    getCurrentWindow().close().catch((err) => console.error("raterm: could not close window", err));
    return;
  }
  renderSessions();
  if (active === s) activate(sessions[Math.min(i, sessions.length - 1)]);
}

function rename(s: Session, tab: HTMLElement) {
  const input = document.createElement("input");
  input.value = s.name;
  tab.replaceChildren(input);
  input.focus();
  input.select();
  // Re-rendering removes the input, which fires blur; only the first outcome counts.
  let finished = false;
  const done = (save: boolean) => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    if (save && name) {
      s.name = name;
      s.renamed = true;
    }
    renderSessions();
  };
  input.onkeydown = (e) => {
    if (e.key === "Enter") done(true);
    if (e.key === "Escape") done(false);
  };
  input.onblur = () => done(true);
  input.onclick = (e) => e.stopPropagation();
}

function renderSessions() {
  tabsEl.replaceChildren(
    ...sessions.map((s) => {
      const tab = document.createElement("div");
      tab.className = "tab";
      tab.title = s.name;
      tab.classList.toggle("active", s === active);
      tab.classList.toggle("exited", s.exited);

      const name = document.createElement("span");
      name.className = "name";
      name.textContent = s.name;

      const x = document.createElement("button");
      x.className = "x";
      x.textContent = "✕";
      x.title = "Close tab";
      x.onclick = (e) => {
        e.stopPropagation();
        close(s);
      };

      tab.append(name, x);
      tab.onclick = () => activate(s);
      tab.ondblclick = () => rename(s, tab);
      // Middle-click closes the tab.
      tab.onauxclick = (e) => {
        if (e.button === 1) close(s);
      };
      return tab;
    }),
  );
}

// ---------- spawn dialog, wiring ----------

function askSpawn() {
  const form = spawnDialog.querySelector("form")!;
  form.reset();
  // returnValue survives between openings; Escape would otherwise reuse the last "ok".
  spawnDialog.returnValue = "";
  spawnDialog.onclose = () => {
    if (spawnDialog.returnValue !== "ok") return active?.term.focus();
    const fd = new FormData(form);
    startSession({
      name: String(fd.get("name")).trim(),
      cwd: String(fd.get("cwd")).trim(),
      command: String(fd.get("command")).trim(),
    });
  };
  spawnDialog.showModal();
}

$("#new-session").onclick = (e) => (e.shiftKey ? askSpawn() : startSession());
new ResizeObserver(() => active?.fit.fit()).observe(termsEl);

const SIDEBAR_KEY = "raterm.sidebarCollapsed";
function setSidebar(collapsed: boolean) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  try {
    localStorage.setItem(SIDEBAR_KEY, collapsed ? "1" : "0");
  } catch (err) {
    // Storage can be unavailable; the remembered state is only a convenience.
    console.debug("raterm: sidebar state not saved", err);
  }
}
try {
  setSidebar(localStorage.getItem(SIDEBAR_KEY) === "1");
} catch (err) {
  console.debug("raterm: sidebar state not restored", err);
}
$("#toggle-sidebar").onclick = () => {
  setSidebar(!document.body.classList.contains("sidebar-collapsed"));
  active?.term.focus();
};

startSession();
