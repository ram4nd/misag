import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { CLAUDE_ICON, fillIcons, icon, type IconName } from "./icons";
import { shellQuote, untypeable } from "./paths";
import { createTree, type Listing } from "./tree";

/**
 * What a tab runs, tagged by `kind`; each kind has only its own fields.
 * Mirrors `layout::TabKind` in src-tauri/src/layout.rs.
 */
type TabKind =
  | { kind: "terminal"; command: string | null; claudeSession?: string | null }
  | { kind: "claude"; claudeSession: string | null };

/**
 * A tab kind once a session has been assigned: a Claude tab always has one. A
 * terminal tab's `claudeSession` is the Claude session to resume at its prompt
 * when it starts (a restored tab that had one open), and null once handed over.
 * Mirrors `commands::RunningKind` in src-tauri/src/commands.rs.
 */
type RunningKind =
  | { kind: "terminal"; command: string | null; claudeSession: string | null }
  | { kind: "claude"; claudeSession: string };

interface Session {
  /** Stable across restarts; a Claude tab's session hook reports under this id. */
  tabId: string;
  /**
   * For Claude tabs, `claudeSession` is the session the tab was launched with;
   * later changes (`/clear`, `/resume`) are tracked by the backend's session hook.
   */
  run: RunningKind;
  /** Backend PTY id; null until pty_spawn resolves, and after it failed. */
  id: number | null;
  /** Counts the tab's starts (restarts included), so checks from an earlier start can tell they are stale. */
  starts: number;
  name: string;
  /** True once the user renamed the tab, so terminal titles stop overwriting it. */
  renamed: boolean;
  /** Directory the tab was opened in; saves use its live directory when they can. */
  cwd?: string;
  term: Terminal;
  fit: FitAddon;
  el: HTMLDivElement;
  exited: boolean;
  closing: boolean;
  /** The running process is Claude started with `--dangerously-skip-permissions`. */
  skipsPermissions: boolean;
  /**
   * For terminal tabs: the Claude session last known to be open in the shell,
   * saved with the tab so a restore reopens it. A check that cannot tell keeps it.
   */
  claudeOpen: string | null;
  /** Until when (ms) a resumed Claude may still be starting, so a shell without it does not mean it quit. */
  resumingUntil: number;
  /** A `claude` seen in the foreground that never reported, and since when (ms). */
  untracked?: { pid: number; since: number };
}

interface SpawnOptions {
  run: TabKind;
  tabId?: string;
  name?: string;
  renamed?: boolean;
  cwd?: string;
  /** Start in the home directory if `cwd` no longer exists. */
  fallbackToHome?: boolean;
}

/** Mirrors `commands::TabLaunch` in src-tauri/src/commands.rs. */
type TabLaunch = RunningKind & {
  tabId: string;
  cols: number;
  rows: number;
  cwd: string | null;
  fallbackToHome: boolean;
};

/** Mirrors `commands::Spawned` in src-tauri/src/commands.rs. */
interface Spawned {
  id: number;
  fellBackToHome: boolean;
  /** The session hook reports for this tab (a Claude tab with a database). */
  tracked: boolean;
  /** Unix seconds, for `claude_reported`. */
  startedAt: number;
  skipsPermissions: boolean;
  resume: ResumeReport | null;
}

/** Mirrors `commands::ResumeReport` in src-tauri/src/commands.rs. */
type ResumeReport = { state: "typed" } | { state: "noConversation" } | { state: "failed"; message: string };

/** Mirrors `commands::ClaudeState` in src-tauri/src/commands.rs. */
type ClaudeState =
  | { state: "open"; session: string }
  | { state: "closed" }
  | { state: "untracked"; pid: number }
  | { state: "unknown" };

/** Mirrors `db::StoreStatus` in src-tauri/src/db.rs. */
type StoreStatus =
  | { state: "saving" }
  | { state: "recovered"; notice: string }
  | { state: "notSaving"; reason: string };

/** Mirrors `claude::HookProblem` in src-tauri/src/claude.rs. */
interface HookProblem {
  /** Identifies the failure, so a dismissed one stays dismissed. */
  at: number;
  message: string;
  /** The hook log's path. */
  log: string;
}

/** Mirrors `commands::StoreReport` in src-tauri/src/commands.rs. */
interface StoreReport {
  store: StoreStatus;
  hookProblem: HookProblem | null;
  /** Why the `claude` wrapper for terminal tabs could not be installed. */
  wrapperProblem: string | null;
}

/** Mirrors `layout::Tab` in src-tauri/src/layout.rs. */
type LayoutTab = TabKind & { tabId: string; name: string; renamed: boolean; cwd: string | null };

/** Mirrors `layout::Layout` in src-tauri/src/layout.rs. */
interface Layout {
  tabs: LayoutTab[];
  active: number;
}

/** Mirrors `settings::Settings` in src-tauri/src/settings.rs. */
interface Settings {
  /** Start Claude tabs with `--dangerously-skip-permissions`. */
  skipPermissions: boolean;
  /** The workspace folder the sidebar shows; null for the home directory. */
  workspace: string | null;
  /** When true, Claude tabs get `autoMemoryEnabled: false`; when false, nothing is passed and the user's Claude config decides. */
  disableAutoMemory: boolean;
}


/** How long after a Claude tab starts its session hook should have reported in. */
const HOOK_REPORT_DELAY_MS = 60_000;
/** Polling interval for the backend's saving and hook status, and for re-checking a hook report. */
const STORE_POLL_MS = 30_000;
/** How often tabs are saved while terminal tabs run, so Claude starting or quitting in one is saved. */
const CLAUDE_POLL_MS = 5_000;
/** How long a `claude` must run without reporting before it is taken to bypass the wrapper. */
const UNTRACKED_GRACE_MS = 10_000;

/** Tab changes are saved this long after the last one, so a burst is one save. */
const SAVE_DELAY_MS = 300;
/** A failed save is tried again this long after it failed. */
const SAVE_RETRY_MS = 5_000;



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
const noticeEl = $<HTMLDivElement>("#notice");

/** A tab that shows a page of the app instead of a terminal. Pages are not saved with the tabs. */
interface Page {
  title: string;
  icon: IconName;
  /** Shown when the stored settings cannot be read, after "Settings could not be read (why), ". */
  unreadable: string;
  el: HTMLElement;
  form: HTMLFormElement;
  /** Its tab in the tab bar while it is open. */
  tabEl: HTMLElement | null;
}
type PageId = "settings" | "workspace";
function page(title: string, icon: IconName, unreadable: string, sel: string): Page {
  const el = $<HTMLElement>(sel);
  return { title, icon, unreadable, el, form: el.querySelector("form")!, tabEl: null };
}
const pages: Record<PageId, Page> = {
  settings: page("Settings", "settings", "so Claude tabs start with permission prompts and auto memory off, and settings cannot be changed.", "#settings"),
  workspace: page(
    "Workspace settings",
    "folder-cog",
    "so the Workspace box shows your home directory and its folder cannot be changed.",
    "#workspace-settings",
  ),
};
const pageIds = Object.keys(pages) as PageId[];
const settingsForm = pages.settings.form;

const sessions: Session[] = [];
/** The last terminal tab selected; while a page (such as Settings) is shown, it stays selected underneath. */
let active: Session | null = null;
/** Pages in the tab bar, in the order opened; their tabs come after the terminal tabs. */
const openPages: PageId[] = [];
/** The page shown instead of a terminal; null when a terminal (or nothing) is shown. */
let shownPage: PageId | null = null;
/** The stored settings, as last loaded or saved; the defaults until the first load. */
let settings: Settings = { skipPermissions: false, workspace: null, disableAutoMemory: false };
let counter = 0;
const encoder = new TextEncoder();

// ---------- sessions ----------

/** A terminal view in a new pane; the pane is removed again if the view cannot be built. */
function createView(): { el: HTMLDivElement; term: Terminal; fit: FitAddon } {
  const el = document.createElement("div");
  el.className = "term";
  termsEl.append(el);
  try {
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
    return { el, term, fit };
  } catch (err) {
    el.remove();
    setNotice("view", `A tab could not be opened: ${errorText(err)}.`);
    throw err;
  }
}

/** Opens a tab and starts its process. */
async function spawn(opts: SpawnOptions) {
  const { el, term, fit } = createView();

  const run: RunningKind =
    opts.run.kind === "claude"
      ? { kind: "claude", claudeSession: opts.run.claudeSession ?? crypto.randomUUID() }
      : { kind: "terminal", command: opts.run.command || null, claudeSession: opts.run.claudeSession ?? null };
  const defaultName =
    run.kind === "claude" ? `Claude ${++counter}` : run.command?.split(" ")[0] || `shell ${++counter}`;
  const s: Session = {
    tabId: opts.tabId ?? crypto.randomUUID(), run,
    id: null, starts: 0, term, fit, el, exited: false, closing: false, skipsPermissions: false,
    claudeOpen: run.kind === "terminal" ? run.claudeSession : null, resumingUntil: 0,
    renamed: opts.renamed ?? Boolean(opts.name),
    cwd: opts.cwd || undefined,
    name: opts.name || defaultName,
  };
  sessions.push(s);
  renderSessions();
  activate(s);
  // The pane must be visible and sized before the PTY is opened with its dimensions.
  fit.fit();

  term.onData((data) => {
    if (s.exited) {
      if (data === "\r") restart(s);
      return;
    }
    write(s, encoder.encode(data));
  });
  term.onBinary((data) => write(s, Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff)));
  term.onResize(({ cols, rows }) => {
    if (s.id === null || s.exited) return;
    invoke("pty_resize", { id: s.id, cols, rows }).catch((err) =>
      console.error("raterm: resize failed", err),
    );
  });
  term.onTitleChange((title) => {
    if (!title || s.renamed || title === s.name) return;
    s.name = title;
    // Claude retitles often while working; only this tab's label changes.
    const tab = tabsEl.children[sessions.indexOf(s)];
    const label = tab?.querySelector(".name");
    if (tab instanceof HTMLElement && label) {
      tab.title = tabTitle(s);
      label.textContent = title;
    }
    scheduleSave();
  });

  await start(s, opts.fallbackToHome ?? false);
}

/**
 * Starts the tab's process. If it fails the tab stays, marked exited with its ids,
 * so saving keeps it for the next restore and Enter can restart it.
 */
async function start(s: Session, fallbackToHome: boolean) {
  const { term } = s;
  const startNo = ++s.starts;
  const onData = new Channel<ArrayBuffer>();
  onData.onmessage = (buf) => term.write(new Uint8Array(buf));
  // Attached before pty_spawn runs, so an immediate exit is not missed.
  const onExit = new Channel<number>();
  onExit.onmessage = (code) =>
    markExited(s, code === 0 ? "process exited" : `process exited with code ${code}`);

  const request: TabLaunch = {
    ...s.run,
    tabId: s.tabId,
    cols: term.cols,
    rows: term.rows,
    cwd: s.cwd ?? null,
    fallbackToHome,
  };
  try {
    const started = await invoke<Spawned>("pty_spawn", { request, onData, onExit });
    s.id = started.id;
    if (started.fellBackToHome) {
      const message = `${s.cwd} no longer exists, so “${s.name}” started in the home directory.`;
      // Claude redraws its screen at once, so the terminal line alone is easy to miss.
      term.write(`\x1b[2m[${message}]\x1b[0m\r\n`);
      setNotice(tabTopic(s, "fallback"), message);
      s.cwd = undefined;
    }
    if (started.tracked) watchHookReport(s, started.startedAt, startNo);
    if (s.run.kind === "terminal") {
      // Handed over once; from now on saves follow whatever Claude session is open in the tab.
      s.run = { ...s.run, claudeSession: null };
      if (started.resume) showResume(s, started.resume);
    }
    if (started.skipsPermissions !== s.skipsPermissions) {
      s.skipsPermissions = started.skipsPermissions;
      renderSessions();
    }
  } catch (err) {
    markExited(s, `could not start: ${errorText(err)}`, true);
  }
  scheduleSave();
}

/** Follows up a terminal tab's saved Claude session, which its start tried to reopen. */
function showResume(s: Session, resume: ResumeReport) {
  const topic = tabTopic(s, "resume");
  if (resume.state === "typed") {
    s.resumingUntil = Date.now() + HOOK_REPORT_DELAY_MS;
    setNotice(topic, null);
  } else if (resume.state === "noConversation") {
    s.claudeOpen = null;
    setNotice(topic, `Claude was open in “${s.name}”, but it saved no conversation there, so it was not reopened.`);
  } else {
    // Kept, so the next start tries again.
    setNotice(topic, `Claude could not be reopened in “${s.name}” (${resume.message}); the next start tries again.`);
  }
}

/** Starts an exited tab again, in the same tab with the same ids (a Claude tab in its current session). */
function restart(s: Session) {
  if (s.closing || !s.exited) return;
  s.exited = false;
  s.id = null;
  s.term.reset();
  renderSessions();
  start(s, true).catch((err) => console.error("raterm: could not restart tab", err));
}

/**
 * Warns when a Claude tab's session hook has not reported since the tab started,
 * so /clear and /resume there would not be remembered. Keeps checking, and clears
 * the warning once the hook reports (e.g. after the user accepts Claude's trust
 * prompt) or the tab goes away.
 */
function watchHookReport(s: Session, since: number, startNo: number) {
  const topic = tabTopic(s, "hook");
  const check = async () => {
    // A restart has its own watch.
    if (s.starts !== startNo) return;
    if (s.closing || s.exited || !sessions.includes(s)) return setNotice(topic, null);
    let reported: boolean;
    try {
      reported = await invoke<boolean>("claude_reported", { tabId: s.tabId, since });
    } catch (err) {
      // Unknown, so neither warn nor clear; try again later.
      console.error("raterm: could not check the session report", err);
      return void setTimeout(check, STORE_POLL_MS);
    }
    if (reported || s.starts !== startNo) return setNotice(topic, null);
    // Keyed by the start, not the text: Claude retitles the tab, which must not undo a dismissal.
    setNotice(
      topic,
      `Claude in “${s.name}” has not reported its session, so /clear and /resume there will not be remembered. ` +
        "Is the folder trusted and are hooks allowed in your Claude settings?",
      true,
      `${since}`,
    );
    setTimeout(check, STORE_POLL_MS);
  };
  setTimeout(check, HOOK_REPORT_DELAY_MS);
}

/**
 * Backend commands that can fail send `{ code, message }` (see `commands::CommandError`);
 * only the message is shown. Anything else (e.g. Tauri's own argument errors) is shown as is.
 */
function errorText(err: unknown): string {
  return typeof err === "object" && err !== null && "message" in err ? String(err.message) : String(err);
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
  s.term.write(`\r\n\x1b[${error ? "31" : "2"}m[${message}; press Enter to restart]\x1b[0m\r\n`);
  renderSessions();
}

function activate(s: Session) {
  if (active !== s) scheduleSave();
  active = s;
  shownPage = null;
  showActive();
  requestAnimationFrame(() => {
    s.fit.fit();
    s.term.focus();
  });
}

/**
 * Marks the visible tab and pane: the shown page if any, else the active terminal.
 * Classes are toggled in place rather than re-rendering, so a click followed by a
 * double-click (rename) lands on the same tab element.
 */
function showActive() {
  const shown = shownPage !== null;
  sessions.forEach((o, i) => {
    o.el.classList.toggle("active", !shown && o === active);
    tabsEl.children[i]?.classList.toggle("active", !shown && o === active);
  });
  for (const id of pageIds) {
    pages[id].el.classList.toggle("active", id === shownPage);
    pages[id].tabEl?.classList.toggle("active", id === shownPage);
  }
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
  clearTabNotices(s);

  renderSessions();
  if (active === s) {
    const next = sessions[Math.min(i, sessions.length - 1)] ?? null;
    active = null;
    // Behind a shown page, the neighbour is only selected.
    if (next && shownPage === null) activate(next);
    else active = next;
  }
  scheduleSave();
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
      scheduleSave();
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
  document.body.classList.toggle("no-tabs", !sessions.length);
  tabsEl.replaceChildren(
    ...sessions.map((s) => {
      const tab = document.createElement("div");
      tab.className = "tab";
      tab.title = tabTitle(s);
      tab.classList.toggle("active", s === active && shownPage === null);
      tab.classList.toggle("exited", s.exited);
      tab.classList.toggle("skips-permissions", s.skipsPermissions);

      const name = document.createElement("span");
      name.className = "name";
      name.textContent = s.name;

      const x = document.createElement("button");
      x.className = "x";
      x.append(icon("x", 12));
      x.title = "Close tab";
      x.onclick = (e) => {
        e.stopPropagation();
        close(s);
      };

      if (s.run.kind === "claude") {
        const mark = document.createElement("span");
        mark.className = "icon";
        mark.innerHTML = CLAUDE_ICON;
        tab.append(mark);
      }
      if (s.skipsPermissions) {
        const badge = document.createElement("span");
        badge.className = "badge";
        badge.textContent = "!";
        tab.append(badge);
      }
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
  for (const id of openPages) {
    pages[id].tabEl = renderPageTab(id);
    tabsEl.append(pages[id].tabEl!);
  }
}

/** The tab's tooltip: its name, and a warning while Claude runs tools without asking. */
function tabTitle(s: Session): string {
  return s.skipsPermissions ? `${s.name} (Claude runs tools without asking: --dangerously-skip-permissions)` : s.name;
}

function renderPageTab(id: PageId): HTMLElement {
  const tab = document.createElement("div");
  tab.className = "tab page-tab";
  tab.title = pages[id].title;
  tab.classList.toggle("active", shownPage === id);
  const mark = document.createElement("span");
  mark.className = "icon";
  mark.append(icon(pages[id].icon));
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = pages[id].title;
  const x = document.createElement("button");
  x.className = "x";
  x.append(icon("x", 12));
  x.title = "Close tab";
  x.onclick = (e) => {
    e.stopPropagation();
    closePage(id);
  };
  tab.append(mark, name, x);
  tab.onclick = () => openPage(id);
  tab.onauxclick = (e) => {
    if (e.button === 1) closePage(id);
  };
  return tab;
}

// ---------- settings ----------

function settingsField<T extends HTMLInputElement | HTMLSelectElement = HTMLInputElement>(name: string) {
  return settingsForm.elements.namedItem(name) as T;
}

/** Shows the stored settings on every page. */
function fillSettingsForms() {
  settingsField("skipPermissions").checked = settings.skipPermissions;
  settingsField<HTMLSelectElement>("disableAutoMemory").value = String(settings.disableAutoMemory ?? false);
  (pages.workspace.form.elements.namedItem("workspace") as HTMLInputElement).value = settings.workspace ?? "";
}

function readSettingsForm(): Settings {
  return {
    ...settings,
    skipPermissions: settingsField("skipPermissions").checked,
    disableAutoMemory: settingsField<HTMLSelectElement>("disableAutoMemory").value === "true",
  };
}

function showPageError(p: Page, message: string | null) {
  const el = p.form.querySelector<HTMLElement>(".error")!;
  el.hidden = !message;
  el.textContent = message ?? "";
}

/**
 * Fills the page from the stored settings, which every launch reads too, so it shows
 * what new tabs will use. Its form (disabled by `openPage`) is enabled once they are
 * read, and stays disabled if they cannot be, so nothing is saved over them.
 */
async function loadPage(p: Page) {
  try {
    await loadSettings();
    fillSettingsForms();
    showWorkspace();
    showPageError(p, null);
    p.form.querySelector("fieldset")!.disabled = false;
  } catch (err) {
    console.error("raterm: could not load settings", err);
    showPageError(p, `Settings could not be read (${errorText(err)}), ${p.unreadable}`);
  }
}

async function loadSettings() {
  settings = await invoke<Settings>("settings_load");
}

/** Saves the settings page; if that fails, the form goes back to the stored settings. */
async function saveSettingsForm() {
  try {
    settings = await invoke<Settings>("settings_save", { settings: readSettingsForm() });
    showPageError(pages.settings, null);
  } catch (err) {
    console.error("raterm: could not save settings", err);
    showPageError(pages.settings, `Settings not saved: ${errorText(err)}`);
  }
  fillSettingsForms();
}

// Loads and saves run one after another, so an earlier one cannot finish last and win.
let settingsQueue = Promise.resolve();
const queueSettings = (task: () => Promise<void>) => {
  // A task that throws is logged, so it cannot stop the ones queued after it.
  settingsQueue = settingsQueue.then(task).catch((err) => console.error("raterm: settings task failed", err));
};
settingsForm.onchange = () => queueSettings(saveSettingsForm);
for (const id of pageIds) pages[id].form.onsubmit = (e) => e.preventDefault();

/** Opens the page's tab (loading the stored settings into it) and shows it. */
function openPage(id: PageId) {
  const p = pages[id];
  if (!openPages.includes(id)) {
    openPages.push(id);
    // Disabled now, not when the queued load starts, so nothing can change before it.
    p.form.querySelector("fieldset")!.disabled = true;
    queueSettings(() => loadPage(p));
  }
  shownPage = id;
  renderSessions();
  showActive();
  // The page, not a control: a key typed while switching must not change a setting.
  p.el.focus();
}

function closePage(id: PageId) {
  const i = openPages.indexOf(id);
  if (i < 0) return;
  openPages.splice(i, 1);
  pages[id].tabEl = null;
  const wasShown = shownPage === id;
  if (wasShown) shownPage = null;
  renderSessions();
  if (wasShown && active) activate(active);
  else showActive();
}

// ---------- notices ----------

/** Messages shown above the terminals, by topic, so a later update replaces an earlier one. */
const notices = new Map<string, { text: string; dismissible: boolean; key: string }>();
/** Per topic, the key of the notice the user dismissed; it stays hidden until the key changes. */
const dismissed = new Map<string, string>();

/**
 * Shows `text` under `topic`, or clears the topic with null (or empty text). `key`
 * identifies the underlying event (the text by default): a dismissed notice comes
 * back only when the key changes, not when polling reports the same thing again.
 * Pass a key when the text can change for the same event.
 */
function setNotice(topic: string, text: string | null, dismissible = true, key = text ?? "") {
  if (!text) {
    notices.delete(topic);
    dismissed.delete(topic);
  } else if (dismissed.get(topic) !== key) {
    notices.set(topic, { text, dismissible, key });
  }
  renderNotices();
}

/** The notice topic for something about one tab; `clearTabNotices` removes them all. */
function tabTopic(s: Session, what: string): string {
  return `tab:${s.tabId}:${what}`;
}

function clearTabNotices(s: Session) {
  const prefix = tabTopic(s, "");
  for (const topic of notices.keys()) if (topic.startsWith(prefix)) notices.delete(topic);
  for (const topic of dismissed.keys()) if (topic.startsWith(prefix)) dismissed.delete(topic);
  renderNotices();
}

function renderNotices() {
  noticeEl.hidden = notices.size === 0;
  noticeEl.querySelector("span")!.textContent = [...notices.values()].map((n) => n.text).join(" ");
  noticeEl.querySelector("button")!.hidden = ![...notices.values()].every((n) => n.dismissible);
}

noticeEl.querySelector("button")!.onclick = () => {
  for (const [topic, n] of notices) {
    if (!n.dismissible) continue;
    notices.delete(topic);
    dismissed.set(topic, n.key);
  }
  renderNotices();
};

// ---------- layout persistence ----------

let saveTimer: ReturnType<typeof setTimeout> | undefined;
// Saving half-restored tabs would lose the rest if the app quit mid-restore.
let restoring = false;
// False once the saved layout could not be loaded or fully restored: saving then would overwrite it.
let canSave = true;
// What was last stored, so an unchanged layout is not written again.
let lastSaved = "";

const savingPaused = () => restoring || !canSave;

function scheduleSave() {
  if (savingPaused()) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveLayout().catch((err) => {
      console.error("raterm: could not save tabs", err);
      // Tried again, so a passing problem (such as a busy database) does not lose the change.
      saveTimer = setTimeout(scheduleSave, SAVE_RETRY_MS);
    });
  }, SAVE_DELAY_MS);
}

/**
 * Saves the open tabs, each in its process's current directory. Does nothing
 * while restoring or after the saved tabs could not be read.
 */
async function saveLayout() {
  clearTimeout(saveTimer);
  if (savingPaused()) return;
  const running = sessions.filter((s) => s.id !== null && !s.exited);
  // Without live directories, each tab keeps the one it was opened in.
  const [cwds, claudes] = await Promise.all([
    liveCwds(running.map((s) => s.id!)),
    claudeStates(running.filter((s) => s.run.kind === "terminal")),
  ]);
  for (const s of sessions) if (s.run.kind === "terminal") followClaude(s, s.id === null ? undefined : claudes[s.id]);
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
    setNotice("save", null);
  } catch (err) {
    setNotice("save", `Could not save tabs: ${errorText(err)}.`);
    throw err;
  }
}

/** The live directories of the given sessions; empty if they cannot be read. */
async function liveCwds(ids: number[]): Promise<Record<number, string>> {
  return invoke<Record<number, string>>("pty_cwds", { ids }).catch((err) => {
    console.error("raterm: could not read tab directories", err);
    return {};
  });
}

/** Whether each of the given tabs has a Claude session open, by PTY id; empty (nothing known) if that cannot be checked. */
async function claudeStates(tabs: Session[]): Promise<Record<number, ClaudeState>> {
  if (!tabs.length) return {};
  const query = tabs.map((s) => ({ id: s.id, tabId: s.tabId }));
  return invoke<Record<number, ClaudeState>>("claude_open_sessions", { tabs: query }).catch((err) => {
    console.error("raterm: could not check for Claude sessions in terminal tabs", err);
    return {};
  });
}

/**
 * Updates the Claude session a terminal tab is known to have open from its latest
 * check (`state`, undefined when it was not checked). Only a clear answer changes
 * it: a tab that could not be checked, or never started, keeps what it had.
 */
function followClaude(s: Session, state: ClaudeState | undefined) {
  // Its shell has exited, so nothing can still run in it.
  if (s.id !== null && s.exited) s.claudeOpen = null;
  if (!state || state.state === "unknown") return;
  if (state.state === "open") {
    s.claudeOpen = state.session;
    s.resumingUntil = 0;
  } else if (Date.now() >= s.resumingUntil) {
    // Not while a resumed Claude may still be starting.
    s.claudeOpen = null;
  }
  if (state.state !== "untracked") return void (s.untracked = undefined);
  if (s.untracked?.pid !== state.pid) s.untracked = { pid: state.pid, since: Date.now() };
  else if (Date.now() - s.untracked.since >= UNTRACKED_GRACE_MS) {
    setNotice(
      tabTopic(s, "untracked"),
      `Claude in “${s.name}” did not start through raterm's claude wrapper (a claude alias, or a PATH that lists another claude first), so it will not be reopened.`,
      true,
      String(state.pid),
    );
  }
}

/** Shows what the backend knows about saving and about the Claude session hook. */
async function showStoreStatus() {
  const report = await invoke<StoreReport>("store_status");
  const { store, hookProblem } = report;
  if (store.state === "recovered") setNotice("store", store.notice);
  else if (store.state === "notSaving") setNotice("store", `Tabs are not being saved: ${store.reason}.`, false);
  else setNotice("store", null);
  for (const id of pageIds) pages[id].form.querySelector<HTMLElement>(".not-saving")!.hidden = store.state !== "notSaving";
  setNotice(
    "hook",
    hookProblem && `Claude session tracking failed (${hookProblem.message}); see ${hookProblem.log}.`,
    true,
    hookProblem ? String(hookProblem.at) : undefined,
  );
  setNotice(
    "wrapper",
    report.wrapperProblem && `Claude sessions started in terminal tabs will not be reopened: ${report.wrapperProblem}.`,
  );
}

/**
 * Reopens the tabs from the last run; with none saved the app starts empty. If the
 * saved tabs cannot be read, or a tab's view cannot be created at all, saving is
 * switched off for this run so the stored layout is kept rather than overwritten.
 * A tab whose process fails to start is not lost: it stays, marked exited, and is
 * saved with its ids.
 */
async function restore() {
  // Sessions from a previous page load (a dev reload) have no tab any more.
  const reset = invoke("pty_reset").catch((err) => {
    console.error("raterm: could not reset sessions", err);
    setNotice("reset", `Leftover sessions could not be stopped (${errorText(err)}); restored Claude tabs may fail to start.`);
  });
  // Loaded while the reset runs; spawning waits for both.
  const load = invoke<Layout>("layout_load");
  let saved: Layout;
  try {
    [, saved] = await Promise.all([reset, load]);
  } catch (err) {
    console.error("raterm: could not load saved tabs", err);
    canSave = false;
    setNotice("load", `Saved tabs could not be read (${errorText(err)}), so changes are not saved this session.`, false);
    return;
  }
  if (!saved.tabs.length) return;
  restoring = true;
  let lost = 0;
  try {
    // All at once: each spawn adds its tab before its first await, so the order holds.
    const results = await Promise.allSettled(
      saved.tabs.map(({ tabId, name, renamed, cwd, ...run }) =>
        spawn({ run, tabId, name, renamed, cwd: cwd ?? undefined, fallbackToHome: true }),
      ),
    );
    results.forEach((r, i) => {
      if (r.status !== "rejected") return;
      lost++;
      console.error("raterm: could not restore tab", saved.tabs[i].name, r.reason);
    });
  } finally {
    restoring = false;
  }
  if (lost) {
    canSave = false;
    setNotice("load", `${lost} saved tab(s) could not be reopened, so changes are not saved this session.`, false);
  }
  if (sessions.length) activate(sessions[Math.min(saved.active, sessions.length - 1)]);
}

// ---------- workspace ----------

const workspaceBox = $<HTMLElement>("#workspace-box");
const tree = createTree(workspaceBox.querySelector<HTMLUListElement>(".tree")!, {
  list: (dir) => invoke<Listing>("fs_list", { dir }),
  errorText,
  onPickFile: insertPath,
  onOpenClaude: (dir) => openClaudeTab(undefined, dir),
});

/** Roots the tree at the workspace folder from the settings (home by default). */
function showWorkspace() {
  tree.setRoot(settings.workspace ?? "~");
}

/** Stores the workspace folder (null for home); a refused folder is shown on the workspace page. */
function setWorkspace(workspace: string | null) {
  queueSettings(async () => {
    try {
      settings = await invoke<Settings>("settings_save", { settings: { ...settings, workspace } });
      showPageError(pages.workspace, null);
      fillSettingsForms();
      showWorkspace();
    } catch (err) {
      console.error("raterm: could not save the workspace folder", err);
      showPageError(pages.workspace, `Workspace folder not saved: ${errorText(err)}`);
    }
  });
}

async function chooseWorkspace() {
  // The listed root has `~` expanded, which the picker needs.
  const picked = await openDialog({ directory: true, multiple: false, defaultPath: tree.listedRoot() });
  if (typeof picked === "string") setWorkspace(picked);
}

/**
 * Types the shell-quoted `path` and a space into the active tab, as if the user had,
 * and shows the tab. Says why in a notice when there is no running tab or the path
 * cannot be typed safely.
 */
function insertPath(path: string) {
  const s = active;
  const shown = JSON.stringify(path);
  if (!s || s.exited || s.id === null) return setNotice("path", `${shown} was not typed: no tab is running.`);
  const why = untypeable(path);
  if (why) return setNotice("path", `${shown} was not typed: ${why}.`);
  invoke("pty_write", { id: s.id, data: Array.from(encoder.encode(`${shellQuote(path)} `)) })
    .then(() => {
      setNotice("path", null);
      if (sessions.includes(s)) activate(s);
    })
    .catch((err) => {
      console.error("raterm: could not type the path", err);
      setNotice("path", `${shown} could not be typed: ${errorText(err)}.`);
    });
}

// ---------- spawn dialog, wiring ----------

/** The active tab's current directory, so a new tab opens next to it. */
async function activeCwd(): Promise<string | undefined> {
  if (!active) return undefined;
  const live = active.id !== null && !active.exited ? (await liveCwds([active.id]))[active.id] : undefined;
  return live ?? active.cwd;
}

/**
 * Opens a tab of `run` in `chosenCwd`, or else next to the active tab. Only a directory
 * taken from the active tab falls back to $HOME if it has gone; a chosen one fails visibly.
 */
function openTab(run: TabKind, name?: string, chosenCwd?: string) {
  (async () => {
    const cwd = chosenCwd || (await activeCwd());
    await spawn({ run, name, cwd, fallbackToHome: !chosenCwd });
  })().catch((err) => console.error(`raterm: could not open ${run.kind} tab`, err));
}

function openTerminalTab(name?: string, chosenCwd?: string) {
  openTab({ kind: "terminal", command: null }, name, chosenCwd);
}

function openClaudeTab(name?: string, chosenCwd?: string) {
  openTab({ kind: "claude", claudeSession: null }, name, chosenCwd);
}

/** Asks for the new tab's kind, name and directory. */
function askTab() {
  const form = spawnDialog.querySelector("form")!;
  form.reset();
  // returnValue survives between openings; Escape would otherwise reuse the last "ok".
  spawnDialog.returnValue = "";
  spawnDialog.onclose = () => {
    if (spawnDialog.returnValue !== "ok") return active?.term.focus();
    const fd = new FormData(form);
    const open = fd.get("kind") === "claude" ? openClaudeTab : openTerminalTab;
    open(String(fd.get("name")).trim(), String(fd.get("cwd")).trim());
  };
  spawnDialog.showModal();
}

$<HTMLButtonElement>("#new-terminal").onclick = (e) => {
  if (e.shiftKey) return askTab();
  openTerminalTab();
};
fillIcons();
for (const el of document.querySelectorAll(".claude-icon")) el.innerHTML = CLAUDE_ICON;
$<HTMLButtonElement>("#open-settings").onclick = () => openPage("settings");
// A hidden terminal has no size to fit to; it is fitted when shown again.
new ResizeObserver(() => {
  if (shownPage === null) active?.fit.fit();
}).observe(termsEl);

/** Remembers an on/off UI choice. Storage can be unavailable; these are only conveniences. */
function remember(key: string, on: boolean) {
  try {
    localStorage.setItem(key, on ? "1" : "0");
  } catch (err) {
    console.debug("raterm: could not remember", key, err);
  }
}

function remembered(key: string): boolean {
  try {
    return localStorage.getItem(key) === "1";
  } catch (err) {
    console.debug("raterm: could not recall", key, err);
    return false;
  }
}

const SIDEBAR_KEY = "raterm.sidebarCollapsed";
function setSidebar(collapsed: boolean) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  remember(SIDEBAR_KEY, collapsed);
}
setSidebar(remembered(SIDEBAR_KEY));
$("#toggle-sidebar").onclick = () => {
  setSidebar(!document.body.classList.contains("sidebar-collapsed"));
  active?.term.focus();
};

/** Wires a sidebar toolbox's header button to collapse it, remembering the choice under `name`. */
function setupToolbox(box: HTMLElement, name: string) {
  const key = `raterm.toolbox.${name}.collapsed`;
  box.classList.toggle("collapsed", remembered(key));
  box.querySelector<HTMLButtonElement>(".toggle")!.onclick = () => {
    const collapsed = !box.classList.contains("collapsed");
    box.classList.toggle("collapsed", collapsed);
    remember(key, collapsed);
  };
}
setupToolbox(workspaceBox, "workspace");
workspaceBox.querySelector<HTMLButtonElement>(".refresh")!.onclick = () => tree.reload();
workspaceBox.querySelector<HTMLButtonElement>(".configure")!.onclick = () => openPage("workspace");
// The path field is read-only: clicking it (or Enter or Space on it) opens the folder picker.
const workspaceInput = pages.workspace.form.elements.namedItem("workspace") as HTMLInputElement;
const pickWorkspace = () =>
  chooseWorkspace().catch((err) => {
    console.error("raterm: could not open the folder picker", err);
    showPageError(pages.workspace, `The folder picker could not be opened: ${errorText(err)}`);
  });
workspaceInput.onclick = pickWorkspace;
workspaceInput.onkeydown = (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  e.preventDefault();
  pickWorkspace();
};
pages.workspace.el.querySelector<HTMLButtonElement>(".home-workspace")!.onclick = () => setWorkspace(null);

/**
 * Saves on window close; the handler finishes before the window is destroyed. If
 * saving fails, the first close is cancelled with a warning, and closing again quits.
 */
async function watchWindowClose() {
  let warned = false;
  await getCurrentWindow().onCloseRequested(async (event) => {
    try {
      await saveLayout();
      warned = false;
    } catch (err) {
      console.error("raterm: could not save tabs", err);
      if (warned) return;
      warned = true;
      event.preventDefault();
      setNotice("save", `Could not save tabs: ${errorText(err)}. Close the window again to quit without saving.`, false);
    }
  });
}

renderSessions();
renderNotices();
// The tree waits for the stored folder; if the settings cannot be read it shows home, and says so.
queueSettings(async () => {
  try {
    await loadSettings();
  } catch (err) {
    console.error("raterm: could not load settings", err);
    setNotice(
      "settings",
      `Settings could not be read (${errorText(err)}), ${pages.workspace.unreadable} Claude tabs start with permission prompts and auto memory off.`,
    );
  }
  showWorkspace();
});
// Async so a failure (e.g. outside Tauri) is logged instead of aborting the UI setup.
watchWindowClose().catch((err) => {
  console.error("raterm: could not watch window close", err);
  setNotice("close", "Tabs are saved after each change, but not when the window closes.");
});
const checkStore = () =>
  showStoreStatus().catch((err) => {
    console.error("raterm: could not read the store status", err);
    setNotice("store", `Could not check whether tabs are being saved (${errorText(err)}).`);
  });
checkStore();
// Session-hook failures can happen at any time, so they are checked on a timer.
setInterval(checkStore, STORE_POLL_MS);
// A Claude starting or quitting in a terminal tab changes nothing else, so it is caught by saving every few seconds (unchanged layouts are not written).
setInterval(() => {
  if (sessions.some((s) => s.run.kind === "terminal" && s.id !== null && !s.exited)) scheduleSave();
}, CLAUDE_POLL_MS);
restore().catch((err) => {
  console.error("raterm: restore failed", err);
  canSave = false;
  setNotice("load", "Saved tabs could not be restored, so changes are not saved this session.", false);
});
