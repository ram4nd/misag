import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

interface Session {
  id: number;
  name: string;
  term: Terminal;
  fit: FitAddon;
  el: HTMLDivElement;
  exited: boolean;
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

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;
const tabsEl = $<HTMLDivElement>("#tabs");
const termsEl = $<HTMLElement>("#terminals");
const spawnDialog = $<HTMLDialogElement>("#spawn-dialog");

const sessions: Session[] = [];
let active: Session | null = null;
let counter = 0;

// ---------- sessions ----------

async function spawn(opts: { name?: string; cwd?: string; command?: string } = {}) {
  const el = document.createElement("div");
  el.className = "term";
  termsEl.append(el);

  const term = new Terminal({
    fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
    fontSize: 14,
    cursorBlink: true,
    scrollback: 10000,
    allowProposedApi: true,
    theme: THEME,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new WebLinksAddon());
  term.open(el);
  el.classList.add("active");
  fit.fit();

  const onData = new Channel<ArrayBuffer>();
  onData.onmessage = (buf) => term.write(new Uint8Array(buf));
  const onExit = new Channel<number>();

  const id = await invoke<number>("pty_spawn", {
    cols: term.cols,
    rows: term.rows,
    cwd: opts.cwd || null,
    command: opts.command || null,
    onData,
    onExit,
  });

  const s: Session = {
    id, term, fit, el, exited: false,
    name: opts.name || opts.command?.split(" ")[0] || `shell ${++counter}`,
  };
  onExit.onmessage = () => {
    s.exited = true;
    term.write("\r\n\x1b[2m[process exited]\x1b[0m\r\n");
    renderSessions();
  };
  term.onData((data) => {
    if (!s.exited) invoke("pty_write", { id, data });
  });
  term.onResize(({ cols, rows }) => {
    if (!s.exited) invoke("pty_resize", { id, cols, rows });
  });
  term.onTitleChange((title) => {
    if (title && !opts.name) {
      s.name = title;
      renderSessions();
    }
  });

  sessions.push(s);
  activate(s);
}

function activate(s: Session) {
  active = s;
  for (const o of sessions) o.el.classList.toggle("active", o === s);
  requestAnimationFrame(() => {
    s.fit.fit();
    s.term.focus();
  });
  renderSessions();
}

async function close(s: Session) {
  await invoke("pty_kill", { id: s.id });
  s.term.dispose();
  s.el.remove();
  const i = sessions.indexOf(s);
  sessions.splice(i, 1);
  if (active === s) {
    const next = sessions[Math.min(i, sessions.length - 1)];
    active = null;
    if (next) activate(next);
  }
  renderSessions();
}

function rename(s: Session, tab: HTMLElement) {
  const input = document.createElement("input");
  input.value = s.name;
  tab.replaceChildren(input);
  input.focus();
  input.select();
  const done = (save: boolean) => {
    if (save && input.value.trim()) s.name = input.value.trim();
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
      // Skip re-rendering the active tab so a double-click lands on the same element.
      tab.onclick = () => s !== active && activate(s);
      tab.ondblclick = () => rename(s, tab);
      // Middle-click closes, as in Chrome.
      tab.onauxclick = (e) => e.button === 1 && close(s);
      return tab;
    }),
  );
}

// ---------- spawn dialog, wiring ----------

function askSpawn() {
  const form = spawnDialog.querySelector("form")!;
  form.reset();
  spawnDialog.onclose = () => {
    if (spawnDialog.returnValue !== "ok") return active?.term.focus();
    const fd = new FormData(form);
    spawn({
      name: String(fd.get("name")).trim(),
      cwd: String(fd.get("cwd")).trim(),
      command: String(fd.get("command")).trim(),
    });
  };
  spawnDialog.showModal();
}

$("#new-session").onclick = (e) => ((e as MouseEvent).shiftKey ? askSpawn() : spawn());
new ResizeObserver(() => active?.fit.fit()).observe(termsEl);

const SIDEBAR_KEY = "raterm.sidebarCollapsed";
function setSidebar(collapsed: boolean) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  try {
    localStorage.setItem(SIDEBAR_KEY, collapsed ? "1" : "0");
  } catch {}
}
try {
  setSidebar(localStorage.getItem(SIDEBAR_KEY) === "1");
} catch {}
$("#toggle-sidebar").onclick = () => {
  setSidebar(!document.body.classList.contains("sidebar-collapsed"));
  active?.term.focus();
};

spawn();
