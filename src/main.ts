import { Channel, invoke } from "@tauri-apps/api/core";
import { WTerm } from "@wterm/dom";
import { homeDir } from "@tauri-apps/api/path";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ask, open as openDialog } from "@tauri-apps/plugin-dialog";
import "@wterm/dom/css";
import { CLAUDE_ICON, fillIcons, icon, type IconName } from "./icons";
import { folderLabel, shellQuote, untypeable } from "./paths";
import { createTree, type Listing } from "./tree";
import { createFlows, type Flow, type Run } from "./flows";
import { createFlowEditor, type FlowEditor } from "./flowEditor";
import { createHistory, type Page as HistoryPage, type Past } from "./history";
import { DRAG_THRESHOLD, forget, remember, remembered, setupToolboxes, slide } from "./toolbox";

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
  /** The user's name for the tab, or else the terminal's latest title (shown in the tooltip). */
  name: string;
  /** True once the user renamed the tab, so terminal titles stop overwriting it. */
  renamed: boolean;
  /** The terminal title starts with a spinner frame: Claude is working, if it runs in the tab (see `showsBusy`). */
  busy: boolean;
  /** Directory the tab was opened in; saves use its live directory when they can. */
  cwd?: string;
  /** The process's current directory, last time it was checked. */
  liveCwd?: string;
  term: WTerm;
  el: HTMLDivElement;
  exited: boolean;
  closing: boolean;
  /** The tab's Claude was started with `--dangerously-skip-permissions`. */
  skipsPermissions: boolean;
  /** For terminal tabs: Claude was in the foreground at the latest clear check. */
  inClaude: boolean;
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
  | { state: "open"; session: string; skipsPermissions: boolean }
  | { state: "closed" }
  | { state: "untracked"; pid: number; skipsPermissions: boolean }
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
  /** Why the `misag` shim for tabs could not be installed. */
  cliProblem: string | null;
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
  /** The terminals' font size in pixels. */
  fontSize: number;
}

/** The terminals' font size, and the range allowed: mirrors `settings::DEFAULT_FONT_SIZE` and `FONT_SIZES`. */
const DEFAULT_FONT_SIZE = 14;
const FONT_SIZES = { min: 8, max: 32 };


/** How long after a Claude tab starts its session hook should have reported in. */
const HOOK_REPORT_DELAY_MS = 60_000;
/** Polling interval for the backend's saving and hook status, and for re-checking a hook report. */
const STORE_POLL_MS = 30_000;
/** How long a `claude` must run without reporting before it is taken to bypass the wrapper. */
const UNTRACKED_GRACE_MS = 10_000;

/** How often the tabs' directories and terminal tabs' Claude are checked (see `pollTabs`). */
const TAB_POLL_MS = 2_000;

/** How often the Session history box, while it can be seen, reads the saved Claude sessions again, as Claude adds to them. */
const HISTORY_POLL_MS = 10_000;

/** Tab changes are saved this long after the last one, so a burst is one save. */
const SAVE_DELAY_MS = 300;
/** A failed save is tried again this long after it failed. */
const SAVE_RETRY_MS = 5_000;


function $<T extends HTMLElement>(sel: string): T {
  const el = document.querySelector<T>(sel);
  if (!el) throw new Error(`misag: missing element ${sel}`);
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
  /** Fills the page when its tab opens, enabling its form; by default from the stored settings (`loadPage`). */
  load?: () => Promise<void>;
  /** Called whenever the pages are redrawn with this one shown. */
  shown?: () => void;
}
/** The fixed pages, plus one editor page per open flow. */
type StaticPageId = "settings" | "workspace" | "flows";
type EditorPageId = `flow:${number}`;
type PageId = StaticPageId | EditorPageId;
const isEditorPage = (id: PageId): id is EditorPageId => id.startsWith("flow:");
function page(title: string, icon: IconName, unreadable: string, sel: string): Page {
  const el = $<HTMLElement>(sel);
  return { title, icon, unreadable, el, form: el.querySelector("form")!, tabEl: null };
}
const pages: Record<StaticPageId, Page> = {
  settings: page("Settings", "settings", "so Claude tabs start with permission prompts and auto memory off, and settings cannot be changed.", "#settings"),
  workspace: page(
    "Workspace settings",
    "folder-cog",
    "so the Workspace box shows your home directory and its folder cannot be changed.",
    "#workspace-settings",
  ),
  flows: page("Flows", "workflow", "", "#flows-settings"),
};
const pageIds = Object.keys(pages) as StaticPageId[];
/** The open flows' editor pages, created when opened and removed when closed. */
const editorPages = new Map<EditorPageId, { page: Page; editor: FlowEditor }>();
function pageOf(id: PageId): Page {
  if (!isEditorPage(id)) return pages[id];
  const editor = editorPages.get(id);
  // Editor pages leave the map only after their tab has closed.
  if (!editor) throw new Error(`misag: no editor page ${id}`);
  return editor.page;
}
const settingsForm = pages.settings.form;

const sessions: Session[] = [];
/** The last terminal tab selected; while a page (such as Settings) is shown, it stays selected underneath. */
let active: Session | null = null;
/** Pages in the tab bar, in the order opened; their tabs come after the terminal tabs. */
const openPages: PageId[] = [];
/** The page shown instead of a terminal; null when a terminal (or nothing) is shown. */
let shownPage: PageId | null = null;
/** The stored settings, as last loaded or saved; the defaults until the first load. */
let settings: Settings = { skipPermissions: false, workspace: null, disableAutoMemory: false, fontSize: DEFAULT_FONT_SIZE };
let counter = 0;
const encoder = new TextEncoder();

// ---------- sessions ----------

/**
 * A terminal view in a new pane; its colors and font are set in style.css. It paints only
 * while shown (see `showActive`), and is sized by the app's own observer, not wterm's.
 */
function createView(): { el: HTMLDivElement; term: WTerm } {
  const el = document.createElement("div");
  el.className = "term";
  termsEl.append(el);
  const term = new WTerm(el, { cursorBlink: true, autoResize: false, renderingPaused: true });
  // Added after wterm's own listener, which keeps a plain click on a link in the terminal and
  // lets Ctrl+click or the keyboard through. Left to the webview, those would open the link in it.
  el.addEventListener("click", (e) => {
    const link = e.target instanceof Element ? e.target.closest<HTMLAnchorElement>("a.term-link") : null;
    if (!link || e.defaultPrevented) return;
    e.preventDefault();
    openUrl(link.href).catch((err) => console.error("misag: could not open link", link.href, err));
  });
  return { el, term };
}

/** Claude titles the terminal `✳ <task>` while idle, and with a spinner frame (`◐ <task>`, `⠂ <task>`) while working. */
const BUSY_TITLE = /^[\u25D0-\u25D3\u2801-\u28FF] /;

/** Opens a tab and starts its process. */
async function spawn(opts: SpawnOptions) {
  const { el, term } = createView();

  const run: RunningKind =
    opts.run.kind === "claude"
      ? { kind: "claude", claudeSession: opts.run.claudeSession ?? crypto.randomUUID() }
      : { kind: "terminal", command: opts.run.command || null, claudeSession: opts.run.claudeSession ?? null };
  const defaultName =
    run.kind === "claude" ? `Claude ${++counter}` : run.command?.split(" ")[0] || `shell ${++counter}`;
  const s: Session = {
    tabId: opts.tabId ?? crypto.randomUUID(), run,
    id: null, starts: 0, term, el, exited: false, closing: false, busy: false, skipsPermissions: false, inClaude: false,
    claudeOpen: run.kind === "terminal" ? run.claudeSession : null, resumingUntil: 0,
    renamed: opts.renamed ?? Boolean(opts.name),
    cwd: opts.cwd || undefined,
    name: opts.name || defaultName,
  };
  const before = { tab: active, page: shownPage };
  sessions.push(s);
  renderSessions();
  activate(s);
  try {
    // Output written before this resolves would be dropped.
    await term.init();
  } catch (err) {
    // Closing the tab already cleaned up after it.
    if (s.closing) return;
    sessions.splice(sessions.indexOf(s), 1);
    el.remove();
    renderSessions();
    if (active === s) showInstead(before.tab, before.page);
    setNotice("view", `A tab could not be opened: ${errorText(err)}.`);
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
  if (active === s && shownPage === null) term.focus();

  term.onData = (data) => {
    if (s.exited) {
      if (data === "\r") restart(s);
      return;
    }
    write(s, encoder.encode(data));
  };
  term.onBinary = (data) => write(s, data);
  // WebKitGTK 2.54 crashes the app (in its callback for the primary selection's
  // targets) pasting a middle click itself, so the terminal does it instead.
  for (const type of ["mousedown", "auxclick"] as const) {
    term.element.addEventListener(type, (e) => {
      if (e.button === 1) e.preventDefault();
    });
  }
  term.element.addEventListener("mouseup", (e) => {
    if (e.button !== 1) return;
    e.preventDefault();
    term.focus();
    // A program tracking the mouse (tmux, vim) gets the click instead, as in xterm;
    // Shift, which wterm does not report, pastes anyway.
    if ((term.bridge?.mouseTracking?.() ?? 0) !== 0 && !e.shiftKey) return;
    void pasteFrom(term, "primary_selection");
  });
  // wterm focuses only on a click, which WebKit does not fire when Claude redrew the
  // pressed row between press and release. So letting go of the left button anywhere on
  // the shown pane focuses it, unless that selected text.
  term.element.addEventListener("mouseup", (e) => {
    if (e.button !== 0) return;
    // After wterm's own click handling, which may have selected a word.
    setTimeout(() => {
      if (active === s && shownPage === null && (getSelection()?.isCollapsed ?? true) && !term.getSelectionText?.()) term.focus();
    });
  });
  // WebKit's menu over the terminal has no Paste, so a right click copies the selected
  // text, or with nothing selected pastes the clipboard, as in PuTTY. The menu stays off
  // either way, also when a program tracking the mouse gets the click.
  term.element.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (term.getSelectionText?.()) {
      // wterm puts its selection on the clipboard as the copy happens.
      document.execCommand("copy");
      getSelection()?.removeAllRanges();
      term.focus();
      return;
    }
    term.focus();
    if ((term.bridge?.mouseTracking?.() ?? 0) !== 0 && !e.shiftKey) return;
    void pasteFrom(term, "clipboard_text");
  });
  term.onResize = (cols, rows) => {
    if (s.id === null || s.exited) return;
    invoke("pty_resize", { id: s.id, cols, rows }).catch((err) =>
      console.error("misag: resize failed", err),
    );
  };
  term.onTitle = (title) => {
    const busy = BUSY_TITLE.test(title);
    if (busy !== s.busy) {
      s.busy = busy;
      updateTab(s);
    }
    if (!title || s.renamed || title === s.name) return;
    s.name = title;
    updateTab(s);
    scheduleSave();
  };

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
  onData.onmessage = (buf) => show(s, new Uint8Array(buf));
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
      show(s, `\x1b[2m[${message}]\x1b[0m\r\n`);
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
  // A full reset (RIS) clears the screen and modes the last process left behind.
  show(s, "\x1bc");
  renderSessions();
  start(s, true).catch((err) => console.error("misag: could not restart tab", err));
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
      console.error("misag: could not check the session report", err);
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

/** Writes to a tab's terminal. wterm rethrows what its callbacks threw while it wrote; the output itself is written. */
function show(s: Session, data: string | Uint8Array) {
  try {
    s.term.write(data);
  } catch (err) {
    console.error("misag: a terminal callback failed", err);
  }
}

function write(s: Session, data: Uint8Array) {
  if (s.id === null || s.exited) return;
  invoke("pty_write", { id: s.id, data: Array.from(data) }).catch((err) =>
    console.error("misag: write failed", err),
  );
}

function markExited(s: Session, message: string, error = false) {
  if (s.closing) return;
  s.exited = true;
  s.busy = false;
  if (s.run.kind === "terminal") {
    s.inClaude = false;
    s.skipsPermissions = false;
  }
  show(s, `\r\n\x1b[${error ? "31" : "2"}m[${message}; press Enter to restart]\x1b[0m\r\n`);
  renderSessions();
}

function activate(s: Session) {
  if (active !== s) scheduleSave();
  active = s;
  shownPage = null;
  showActive();
  showHistory();
  // At once, so keys typed before the next frame reach it; again once it is fitted.
  s.term.focus();
  requestAnimationFrame(() => {
    s.term.fit();
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
    // A hidden terminal still takes in its output, but painting it would be wasted work.
    o.term.setRenderingPaused(shown || o !== active);
    tabsEl.children[i]?.classList.toggle("active", !shown && o === active);
  });
  for (const id of [...pageIds, ...editorPages.keys()]) {
    pageOf(id).el.classList.toggle("active", id === shownPage);
    pageOf(id).tabEl?.classList.toggle("active", id === shownPage);
  }
  if (shownPage !== null) pageOf(shownPage).shown?.();
  showTabRuns();
}

/** Shows the given tab or page again (as they were before a tab that could not be opened), or nothing. */
function showInstead(tab: Session | null, page: PageId | null) {
  active = null;
  if (!tab || !sessions.includes(tab)) return showActive();
  if (page === null) return activate(tab);
  active = tab;
  shownPage = page;
  showActive();
}

function close(s: Session) {
  if (s.closing) return;
  s.closing = true;
  // Its run can never be answered again: the tab's id is not reused.
  invoke("flow_stop_tab", { tabId: s.tabId }).catch((err) => console.error("misag: could not end the tab's flow run", err));
  // Not one of a closed tab while the stop is on its way; if it fails, the next poll shows it as one.
  runs = runs.filter((r) => r.tabId !== s.tabId);
  const i = sessions.indexOf(s);
  sessions.splice(i, 1);
  // A missing session (already exited) is a no-op on the backend.
  if (s.id !== null) {
    invoke("pty_kill", { id: s.id }).catch((err) => console.error("misag: kill failed", err));
  }
  s.term.destroy();
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
  showHistory();
  showTabRuns();
  scheduleSave();
}

function rename(s: Session, tab: HTMLElement) {
  const input = document.createElement("input");
  // What the tab shows; leaving it unchanged keeps the tab following its folder.
  const shown = tabLabel(s);
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
      tab.classList.toggle("busy", showsBusy(s));

      const name = document.createElement("span");
      name.className = "name";
      name.textContent = tabLabel(s);
      tab.classList.toggle("needs-you", waitingTabs.has(s.tabId));

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
  for (const id of openPages) {
    pageOf(id).tabEl = renderPageTab(id);
    tabsEl.append(pageOf(id).tabEl!);
  }
}

/** The tab being dragged, if any. */
let draggedTab: HTMLElement | null = null;

/** The terminal tab of `s` in the tab bar, if it is still open. */
const tabOf = (s: Session) => tabsEl.children[sessions.indexOf(s)] as HTMLElement | undefined;

/** Moves the tab of `s` to place `to` among the terminal tabs, sliding the tab it passes into its new place. */
function moveTab(s: Session, to: number) {
  const from = sessions.indexOf(s);
  const tab = tabOf(s)!;
  const passed = tabsEl.children[to] as HTMLElement;
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
      const prev = tabsEl.children[i - 1] as HTMLElement | undefined;
      const next = i + 1 < sessions.length ? (tabsEl.children[i + 1] as HTMLElement) : undefined;
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
    scheduleSave();
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", up);
}

/** The tab's directory: its live one, else the one it was opened in. */
const tabDir = (s: Session) => s.liveCwd ?? s.cwd;

/** The tab's label: the user's name for it, else its folder, else its title until the folder is known. */
function tabLabel(s: Session): string {
  const dir = tabDir(s);
  return s.renamed || !dir ? s.name : folderLabel(dir, home);
}

/** The tab's tooltip: its title and directory, and a warning while Claude runs tools without asking. */
function tabTitle(s: Session): string {
  const dir = tabDir(s);
  const text = dir ? `${s.name}\n${dir}` : s.name;
  return s.skipsPermissions ? `${text}\n(Claude runs tools without asking: --dangerously-skip-permissions)` : text;
}

/** Whether the tab shows Claude working: another tool's spinner in a plain shell does not count. */
function showsBusy(s: Session): boolean {
  return s.busy && !s.exited && (s.run.kind === "claude" || s.inClaude);
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

/**
 * Refreshes one tab's label, tooltip and icon in place; titles, directories and
 * Claude change often. A tab being renamed is left alone, so its input stays open;
 * finishing the rename renders it again.
 */
function updateTab(s: Session) {
  const tab = tabsEl.children[sessions.indexOf(s)];
  const label = tab?.querySelector(".name");
  const mark = tab?.querySelector(".icon");
  if (!(tab instanceof HTMLElement) || !label || !mark) return;
  tab.title = tabTitle(s);
  tab.classList.toggle("busy", showsBusy(s));
  label.textContent = tabLabel(s);
  mark.replaceWith(tabIcon(s));
}

/** The home directory, shown as `~`; null until known, or if it cannot be. */
let home: string | null = null;
homeDir().then(
  (dir) => {
    home = dir;
    sessions.forEach(updateTab);
  },
  (err) => console.error("misag: could not find the home directory", err),
);

/** True while `pollTabs` waits for the backend, so a slow check is not started twice. */
let polling = false;

/**
 * Checks the running tabs' directories and the Claude in terminal tabs, so labels
 * and icons follow them. When either changed it saves (a Claude starting or
 * quitting in a terminal tab changes nothing else) and redraws the Session history
 * box, whose marks of the tabs that have a session open may have changed. Answers for a tab that exited,
 * restarted or closed meanwhile are dropped.
 */
async function pollTabs() {
  if (polling) return;
  const running = sessions.filter((s) => s.id !== null && !s.exited).map((s) => ({ s, id: s.id!, starts: s.starts }));
  if (!running.length) return;
  polling = true;
  try {
    const terminals = running.filter(({ s }) => s.run.kind === "terminal");
    const [cwds, claudes] = await Promise.all([
      liveCwds(running.map(({ id }) => id)),
      claudeStates(terminals.map(({ s }) => s)),
    ]);
    let changed = false;
    for (const { s, id, starts } of running) {
      if (s.exited || s.starts !== starts || !sessions.includes(s)) continue;
      const before = { dir: s.liveCwd, claude: s.claudeOpen };
      if (s.run.kind === "terminal") followClaude(s, claudes[id]);
      if (cwds[id]) s.liveCwd = cwds[id];
      if (s.liveCwd === before.dir && s.claudeOpen === before.claude) continue;
      changed = true;
      updateTab(s);
    }
    if (changed) {
      showHistory();
      scheduleSave();
    }
  } catch (err) {
    console.error("misag: could not check the tabs", err);
  } finally {
    polling = false;
  }
}

function renderPageTab(id: PageId): HTMLElement {
  const p = pageOf(id);
  const tab = document.createElement("div");
  tab.className = "tab page-tab";
  tab.title = p.title;
  tab.classList.toggle("active", shownPage === id);
  const mark = document.createElement("span");
  mark.className = "icon";
  mark.append(icon(p.icon));
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = p.title;
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
  settingsField("fontSize").value = String(settings.fontSize ?? DEFAULT_FONT_SIZE);
  (pages.workspace.form.elements.namedItem("workspace") as HTMLInputElement).value = settings.workspace ?? "";
}

function readSettingsForm(): Settings {
  return {
    ...settings,
    skipPermissions: settingsField("skipPermissions").checked,
    disableAutoMemory: settingsField<HTMLSelectElement>("disableAutoMemory").value === "true",
    fontSize: Number(settingsField("fontSize").value),
  };
}

/** Sets the terminals' font size from the settings; a hidden terminal measures it again when shown (see `activate`). */
function applyFontSize() {
  document.documentElement.style.setProperty("--terminal-font-size", `${settings.fontSize ?? DEFAULT_FONT_SIZE}px`);
  if (shownPage === null) active?.term.fit();
}

/** Enables or disables every control on the page (each part of it is a fieldset). */
function enablePage(p: Page, on: boolean) {
  for (const fieldset of p.el.querySelectorAll("fieldset")) fieldset.disabled = !on;
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
    enablePage(p, true);
  } catch (err) {
    console.error("misag: could not load settings", err);
    showPageError(p, `Settings could not be read (${errorText(err)}), ${p.unreadable}`);
  }
}

async function loadSettings() {
  settings = await invoke<Settings>("settings_load");
  applyFontSize();
}

/** Saves the settings page; if that fails, the form goes back to the stored settings. */
async function saveSettingsForm() {
  const next = readSettingsForm();
  const { min, max } = FONT_SIZES;
  if (!Number.isInteger(next.fontSize) || next.fontSize < min || next.fontSize > max) {
    showPageError(pages.settings, `Settings not saved: the font size must be ${min} to ${max}.`);
  } else {
    try {
      settings = await invoke<Settings>("settings_save", { settings: next });
      showPageError(pages.settings, null);
      applyFontSize();
    } catch (err) {
      console.error("misag: could not save settings", err);
      showPageError(pages.settings, `Settings not saved: ${errorText(err)}`);
    }
  }
  fillSettingsForms();
}

// Loads and saves run one after another, so an earlier one cannot finish last and win.
let settingsQueue = Promise.resolve();
const queueSettings = (task: () => Promise<void>) => {
  // A task that throws is logged, so it cannot stop the ones queued after it.
  settingsQueue = settingsQueue.then(task).catch((err) => console.error("misag: settings task failed", err));
};
settingsForm.onchange = () => queueSettings(saveSettingsForm);
for (const id of pageIds) pages[id].form.onsubmit = (e) => e.preventDefault();

/** Opens the page's tab (loading the stored settings into it) and shows it. */
function openPage(id: PageId) {
  const p = pageOf(id);
  if (!openPages.includes(id)) {
    openPages.push(id);
    // Disabled now, not when the load starts, so nothing can change before it.
    enablePage(p, false);
    if (p.load) p.load();
    else queueSettings(() => loadPage(p));
  }
  shownPage = id;
  renderSessions();
  showActive();
  // The page, not a control: a key typed while switching must not change a setting.
  p.el.focus();
}

/**
 * Closes the page's tab. A flow editor's waiting changes are saved first, and if
 * they cannot be, its tab stays open showing why.
 */
function closePage(id: PageId) {
  const editor = isEditorPage(id) ? editorPages.get(id) : undefined;
  if (!editor) return removePage(id);
  editor.editor.flush().then((saved) => {
    if (saved) removePage(id);
    else if (openPages.includes(id)) openPage(id);
  });
}

/** Closes the page's tab right away; a flow editor's page goes with it. */
function removePage(id: PageId) {
  const i = openPages.indexOf(id);
  if (i < 0) return;
  openPages.splice(i, 1);
  pageOf(id).tabEl = null;
  if (isEditorPage(id)) {
    editorPages.get(id)!.page.el.remove();
    editorPages.delete(id);
  }
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
      console.error("misag: could not save tabs", err);
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
  if (!ids.length) return {};
  return invoke<Record<number, string>>("pty_cwds", { ids }).catch((err) => {
    console.error("misag: could not read tab directories", err);
    return {};
  });
}

/** Whether each of the given tabs has a Claude session open, by PTY id; empty (nothing known) if that cannot be checked. */
async function claudeStates(tabs: Session[]): Promise<Record<number, ClaudeState>> {
  if (!tabs.length) return {};
  const query = tabs.map((s) => ({ id: s.id, tabId: s.tabId }));
  return invoke<Record<number, ClaudeState>>("claude_open_sessions", { tabs: query }).catch((err) => {
    console.error("misag: could not check for Claude sessions in terminal tabs", err);
    return {};
  });
}

/**
 * Updates the Claude session a terminal tab is known to have open from its latest
 * check (`state`, undefined when it was not checked), and its icon and tooltip with
 * it. Only a clear answer changes them: a tab that could not be checked, or never
 * started, keeps what it had.
 */
function followClaude(s: Session, state: ClaudeState | undefined) {
  // Its shell has exited, so nothing can still run in it, whatever a check that
  // started earlier says.
  if (s.id !== null && s.exited) return void (s.claudeOpen = null);
  if (!state || state.state === "unknown") return;
  showClaudeIn(s, state.state !== "closed", state.state !== "closed" && state.skipsPermissions);
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
      `Claude in “${s.name}” did not start through Mischief of Agents's claude wrapper (a claude alias, or a PATH that lists another claude first), so it will not be reopened.`,
      true,
      String(state.pid),
    );
  }
}

/** Shows whether Claude runs in a terminal tab, and whether it skips permissions, in its icon and tooltip. */
function showClaudeIn(s: Session, inClaude: boolean, skipsPermissions: boolean) {
  if (inClaude === s.inClaude && skipsPermissions === s.skipsPermissions) return;
  // A Claude that quit mid-turn leaves its spinner in the title.
  if (!inClaude) s.busy = false;
  s.inClaude = inClaude;
  s.skipsPermissions = skipsPermissions;
  updateTab(s);
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
  setNotice("cli", report.cliProblem && `misag flow is not available in tabs: ${report.cliProblem}.`);
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
    console.error("misag: could not reset sessions", err);
    setNotice("reset", `Leftover sessions could not be stopped (${errorText(err)}); restored Claude tabs may fail to start.`);
  });
  // Loaded while the reset runs; spawning waits for both.
  const load = invoke<Layout>("layout_load");
  let saved: Layout;
  try {
    [, saved] = await Promise.all([reset, load]);
  } catch (err) {
    console.error("misag: could not load saved tabs", err);
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
      console.error("misag: could not restore tab", saved.tabs[i].name, r.reason);
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
      console.error("misag: could not save the workspace folder", err);
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
      console.error("misag: could not type the path", err);
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
  })().catch((err) => console.error(`misag: could not open ${run.kind} tab`, err));
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
// A hidden terminal has no size to fit to; it is fitted when shown again. While the
// sidebar's edge is dragged, it is fitted once let go, so the PTY is not resized at every move.
new ResizeObserver(() => {
  if (shownPage === null && !document.body.classList.contains("resizing-sidebar")) active?.term.fit();
}).observe(termsEl);

const SIDEBAR_KEY = "misag.sidebarCollapsed";
function setSidebar(collapsed: boolean) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  remember(SIDEBAR_KEY, collapsed);
}
setSidebar(remembered(SIDEBAR_KEY));
/** The sidebar's width (px) in localStorage, and its default and limits. */
const SIDEBAR_WIDTH_KEY = "misag.sidebarWidth";
const SIDEBAR_WIDTH = 240;
const SIDEBAR_MIN = 160;
const SIDEBAR_MAX = 600;
/** The narrowest (px) the sidebar may squeeze the terminals to. */
const TERMINALS_MIN = 320;
/** The width chosen for the sidebar; a narrow window shows it narrower, until the window grows again. */
let chosenSidebarWidth = SIDEBAR_WIDTH;
/** The width the sidebar is shown at. */
let sidebarWidth = SIDEBAR_WIDTH;
const sidebarResizer = $<HTMLElement>("#sidebar-resizer");
sidebarResizer.setAttribute("aria-valuemin", String(SIDEBAR_MIN));
/** The widest the sidebar may be in this window. */
const sidebarMax = () => Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, window.innerWidth - TERMINALS_MIN));
/** Shows the sidebar at `px`, kept within its limits. */
function showSidebarWidth(px: number) {
  const max = sidebarMax();
  sidebarWidth = Math.round(Math.min(max, Math.max(SIDEBAR_MIN, px)));
  $("#sidebar").style.setProperty("--sidebar-width", `${sidebarWidth}px`);
  sidebarResizer.setAttribute("aria-valuenow", String(sidebarWidth));
  sidebarResizer.setAttribute("aria-valuetext", `${sidebarWidth} pixels`);
  sidebarResizer.setAttribute("aria-valuemax", String(max));
}
/** Sets the sidebar's width and remembers it. */
function chooseSidebarWidth(px: number) {
  showSidebarWidth(px);
  chosenSidebarWidth = sidebarWidth;
  try {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth));
  } catch (err) {
    console.debug("misag: could not remember the sidebar width", err);
  }
}
/** Puts the sidebar back at its default width and forgets the one chosen. */
function resetSidebarWidth() {
  forget(SIDEBAR_WIDTH_KEY);
  chosenSidebarWidth = SIDEBAR_WIDTH;
  showSidebarWidth(SIDEBAR_WIDTH);
}
{
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
  } catch (err) {
    console.debug("misag: could not recall the sidebar width", err);
  }
  const px = stored ? Number(stored) : NaN;
  chosenSidebarWidth = Number.isFinite(px) ? Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, px)) : SIDEBAR_WIDTH;
  showSidebarWidth(chosenSidebarWidth);
}
window.addEventListener("resize", () => showSidebarWidth(chosenSidebarWidth));
sidebarResizer.onmousedown = (down) => {
  if (down.button !== 0) return;
  down.preventDefault();
  const start = sidebarWidth;
  // The terminals are fitted once it is let go (see the ResizeObserver above), not at every move.
  document.body.classList.add("resizing-sidebar");
  const move = (e: MouseEvent) => {
    // Let go outside the window, where the mouseup went elsewhere.
    if (!(e.buttons & 1)) return end();
    showSidebarWidth(start + e.clientX - down.clientX);
  };
  const end = () => {
    document.removeEventListener("mousemove", move);
    document.removeEventListener("mouseup", up);
    window.removeEventListener("blur", end);
    document.body.classList.remove("resizing-sidebar");
    chooseSidebarWidth(sidebarWidth);
    if (shownPage === null) active?.term.fit();
    active?.term.focus();
  };
  const up = (e: MouseEvent) => {
    showSidebarWidth(start + e.clientX - down.clientX);
    end();
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", up);
  window.addEventListener("blur", end);
};
/** Left and Right move the edge 16px; Home and End take it to the narrowest and widest. */
const SIDEBAR_KEYS: Record<string, () => number> = {
  ArrowLeft: () => sidebarWidth - 16,
  ArrowRight: () => sidebarWidth + 16,
  Home: () => SIDEBAR_MIN,
  End: sidebarMax,
};
sidebarResizer.onkeydown = (e) => {
  const to = SIDEBAR_KEYS[e.key];
  if (!to) return;
  e.preventDefault();
  chooseSidebarWidth(to());
};
sidebarResizer.ondblclick = resetSidebarWidth;
$("#toggle-sidebar").onclick = () => {
  setSidebar(!document.body.classList.contains("sidebar-collapsed"));
  reloadHistoryIfShown();
  active?.term.focus();
};

workspaceBox.querySelector<HTMLButtonElement>(".refresh")!.onclick = () => tree.reload();
workspaceBox.querySelector<HTMLButtonElement>(".configure")!.onclick = () => openPage("workspace");
const flowsBox = $<HTMLElement>("#flows-box");
const flows = createFlows({
  box: flowsBox,
  page: pages.flows.el,
  errorText,
  showError: (message) => showPageError(pages.flows, message),
  onOpen: openFlowEditor,
  onChange: followFlows,
  onRun: startFlow,
  onStop: stopRun,
  onResume: async (run) => flows.showBoxError(await resumeRun(run)),
  onContinue: async (run, output) => flows.showBoxError(await continueRun(run, output)),
});

/** What a terminal can paste: the backend command that reads it, and its name. */
const PASTE_SOURCES = { primary_selection: "primary selection", clipboard_text: "clipboard" } as const;

/**
 * Types the primary selection (what is selected anywhere on the desktop) or the
 * clipboard into the terminal: in brackets if its program asked for them, with line
 * breaks as Enter sends them and no other control characters, so the text cannot end
 * the brackets early or send keys of its own. Without brackets each line runs as it
 * arrives, so more than one line is pasted only if the person agrees.
 */
async function pasteFrom(term: WTerm, source: keyof typeof PASTE_SOURCES) {
  let text: string | null;
  try {
    text = await invoke<string | null>(source);
  } catch (err) {
    console.error(`misag: could not read the ${PASTE_SOURCES[source]}`, err);
    setNotice("paste", `Could not paste the ${PASTE_SOURCES[source]}: ${errorText(err)}.`);
    return;
  }
  setNotice("paste", null);
  if (!text) return;
  const clean = text.replace(/\r\n?|\n/g, "\r").replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/g, "");
  const bracketed = term.bridge?.bracketedPaste() ?? false;
  if (!bracketed && clean.includes("\r")) {
    const sure = await ask("The text has more than one line, and the program in this tab runs each line as it is pasted. Paste it?", { title: "Paste", kind: "warning" });
    term.focus();
    if (!sure) return;
  }
  term.onData?.(bracketed ? `\x1b[200~${clean}\x1b[201~` : clean);
}

/** How long after pasting a prompt Claude gets the Enter that sends it, so the paste is not taken as typing. */
const PASTE_SUBMIT_MS = 150;
/**
 * For a flow whose Start clears: how often to ask whether Claude has reported the
 * clear (the SessionStart hook does), and how long to wait for it before the run
 * is stopped.
 */
const CLEAR_POLL_MS = 250;
const CLEAR_TIMEOUT_MS = 30_000;
/** Tabs a flow is being started in (through its clear), so ▶ is not taken twice. */
const startingTabs = new Set<string>();
/** Tabs whose flow run waits for the person, marked in the tab bar. */
const waitingTabs = new Set<string>();
/** The active runs, as last read. */
let runs: Run[] = [];
/** Until the saved tabs are back, a run's tab may just not be open yet. */
let tabsRestored = false;

/**
 * Shows the active tab's runs, their flows marked in the toolbox and their steps in
 * the editors, and in the toolbox the runs whose tab is gone, so they can be stopped.
 */
function showTabRuns() {
  const here = runs.filter((r) => r.tabId === active?.tabId);
  const orphans = tabsRestored ? runs.filter((r) => !sessions.some((s) => s.tabId === r.tabId)) : [];
  flows.showRuns(here, orphans);
  for (const [id, { editor }] of editorPages) editor.showRun(here.find((r) => `flow:${r.flowId}` === id) ?? null);
}

/**
 * Types `text` into the tab as one paste and sends it, as if the person had. The
 * backend drops control characters from prompts, so the text cannot end the paste.
 * If Claude is busy (mid-turn, or in a menu), the paste lands there instead. Claude
 * is checked before the paste and again before the Enter: in a terminal tab it may
 * have been suspended or quit meanwhile, and the shell must not run a flow's text.
 * Resolves to whether it was sent; false if Claude had left, when it was not.
 */
async function typePrompt(s: Session, text: string): Promise<boolean> {
  const left = () => {
    console.warn(`misag: Claude left ${tabLabel(s)} before its flow prompt was sent; it was not sent`);
    return false;
  };
  if (!(await claudeLive(s))) return left();
  write(s, encoder.encode(`\x1b[200~${text}\x1b[201~`));
  await new Promise((resolve) => setTimeout(resolve, PASTE_SUBMIT_MS));
  if (!(await claudeLive(s))) return left();
  write(s, encoder.encode("\r"));
  return true;
}

/** Why a prompt typed into `s` was not sent, for the person. */
const notSent = (s: Session) => `Claude left ${tabLabel(s)} before the prompt was sent, so it was not sent`;

/** Mirrors `runs::Started` in src-tauri/src/runs.rs. */
interface Started {
  runId: number;
  clear: boolean;
  prompt: string | null;
}

/** Mirrors `runs::Cleared`. */
type Cleared = { state: "pending" | "gone" } | { state: "ready"; prompt: string | null };

/**
 * Whether Claude runs in the tab right now, so a prompt typed there goes to Claude
 * and not to a shell. A terminal tab is asked now, not trusted to its last poll.
 */
async function claudeLive(s: Session): Promise<boolean> {
  if (s.exited || s.closing || s.id === null) return false;
  if (s.run.kind === "claude") return true;
  return (await claudeStates([s]))[s.id]?.state === "open";
}

async function startFlow(flow: Flow) {
  const s = active;
  // A start already under way here (it may be waiting for its clear) is the one.
  if (!s || startingTabs.has(s.tabId)) return;
  startingTabs.add(s.tabId);
  try {
    await startIn(s, flow);
  } finally {
    startingTabs.delete(s.tabId);
  }
  await pollRuns();
}

async function startIn(s: Session, flow: Flow) {
  if (!(await claudeLive(s))) {
    flows.showBoxError("Flows run in a Claude tab: select one (or a terminal tab running claude) and press ▶ again.");
    return;
  }
  let started: Started;
  try {
    // The run starts from the stored flow, so the editor's latest change goes first;
    // one that cannot be saved says so in the editor.
    await editorPages.get(`flow:${flow.id}`)?.editor.flush();
    // The backend takes the tab before anything is typed, so a start it refuses
    // (the tab runs a flow, the flow is not ready) clears nothing.
    started = await invoke<Started>("flow_start", { flowId: flow.id, tabId: s.tabId });
  } catch (err) {
    console.error("misag: could not start the flow", err);
    flows.showBoxError(`Could not start "${flow.name}": ${errorText(err)}`);
    return;
  }
  flows.showBoxError(null);
  if (!started.clear) {
    if (started.prompt !== null && !(await typePrompt(s, started.prompt))) {
      flows.showBoxError(`${notSent(s)}. Start Claude there and pick an output to go on, or stop the run.`);
    }
    return;
  }
  await pollRuns();
  if (showsBusy(s)) {
    // Claude would take /clear only after its turn, which may outlast the wait.
    await stopStart(started.runId, `Claude is working in ${tabLabel(s)}, so "${flow.name}" was not started; press ▶ again when it is idle.`);
    return;
  }
  if (!(await typePrompt(s, "/clear"))) {
    await stopStart(started.runId, `Claude left ${tabLabel(s)} before /clear was sent, so "${flow.name}" was stopped.`);
    return;
  }
  for (let waited = 0; waited < CLEAR_TIMEOUT_MS; waited += CLEAR_POLL_MS) {
    await new Promise((resolve) => setTimeout(resolve, CLEAR_POLL_MS));
    let cleared: Cleared;
    try {
      cleared = await invoke<Cleared>("flow_cleared", { runId: started.runId });
    } catch (err) {
      console.error("misag: could not check the flow's clear", err);
      flows.showBoxError(`Could not go on with "${flow.name}" after /clear: ${errorText(err)}. Stop the run and press ▶ again.`);
      return;
    }
    // Stopped meanwhile: nothing to do.
    if (cleared.state === "gone") return;
    if (cleared.state === "ready") {
      if (cleared.prompt !== null && !(await typePrompt(s, cleared.prompt))) {
        flows.showBoxError(`${notSent(s)}. Start Claude there and pick an output to go on, or stop the run.`);
      }
      return;
    }
  }
  await stopStart(started.runId, `Claude did not clear the conversation in ${tabLabel(s)} within ${CLEAR_TIMEOUT_MS / 1000} seconds, so "${flow.name}" was stopped; press ▶ again when it is idle.`);
}

/** Stops a run whose start could not go through its clear, saying why. */
async function stopStart(runId: number, why: string) {
  try {
    await invoke("flow_stop", { runId });
    flows.showBoxError(why);
  } catch (err) {
    console.error("misag: could not stop the run", err);
    flows.showBoxError(`${why} It could not be stopped (${errorText(err)}); stop it with ■.`);
  }
}

/**
 * Moves the run on with `go`, typing the prompt it returns into the run's tab;
 * resolves to why it could not, for the run bar it was picked in, or null.
 */
async function goOn(run: Run, go: () => Promise<string | null>): Promise<string | null> {
  const s = sessions.find((s) => s.tabId === run.tabId);
  let error: string | null = null;
  if (!s || !(await claudeLive(s))) {
    error = `Claude is not running in ${s ? tabLabel(s) : "the run's tab"}; start it there first, or stop the run.`;
  } else {
    try {
      const prompt = await go();
      if (prompt !== null && !(await typePrompt(s, prompt))) error = `${notSent(s)}; start Claude there and go on again.`;
    } catch (err) {
      console.error("misag: could not continue the run", err);
      error = `Could not go on: ${errorText(err)}`;
    }
  }
  await pollRuns();
  return error;
}

/** Goes on as `output`. */
function continueRun(run: Run, output: string): Promise<string | null> {
  return goOn(run, () => invoke<string | null>("flow_continue", { runId: run.id, output }));
}

/** Has Claude go on with the run's step that needs attention. */
function resumeRun(run: Run): Promise<string | null> {
  return goOn(run, () => invoke<string>("flow_resume", { runId: run.id }));
}

async function stopRun(run: Run) {
  try {
    await invoke("flow_stop", { runId: run.id });
    flows.showBoxError(null);
  } catch (err) {
    console.error("misag: could not stop the run", err);
    flows.showBoxError(`Could not stop the run: ${errorText(err)}`);
  }
  await pollRuns();
}

/** The error pollRuns shows, so a later good poll clears only its own. */
let runsError: string | null = null;

/** Shows the active tab's runs, and marks the tabs whose run waits for the person. */
async function pollRuns() {
  try {
    runs = await invoke<Run[]>("flow_runs");
    if (runsError !== null) flows.showBoxError(null);
    runsError = null;
    showTabRuns();
    waitingTabs.clear();
    for (const run of runs) {
      if (run.status !== "running") waitingTabs.add(run.tabId);
    }
    sessions.forEach((s, i) => tabsEl.children[i]?.classList.toggle("needs-you", waitingTabs.has(s.tabId)));
  } catch (err) {
    console.error("misag: could not read the flow runs", err);
    runsError = `Flow runs could not be read: ${errorText(err)}`;
    flows.showBoxError(runsError);
  }
}

/** Opens the flow's editor tab, creating it the first time. */
function openFlowEditor(flow: Flow) {
  const id: EditorPageId = `flow:${flow.id}`;
  if (!editorPages.has(id)) {
    const editor = createFlowEditor(flow.id, { errorText, onContinue: continueRun, onResume: resumeRun });
    termsEl.append(editor.el);
    const page: Page = {
      title: flow.name,
      icon: "workflow",
      unreadable: "",
      el: editor.el,
      form: editor.el.querySelector("form")!,
      tabEl: null,
      load: async () => {
        if (await editor.load()) enablePage(page, true);
      },
      shown: editor.shown,
    };
    editorPages.set(id, { page, editor });
    showTabRuns();
  }
  openPage(id);
}

/** Keeps editor tabs in step with the stored flows: renamed ones are retitled, deleted ones closed. */
function followFlows(stored: Flow[]) {
  for (const [id, { page }] of editorPages) {
    const flow = stored.find((f) => `flow:${f.id}` === id);
    if (flow) {
      page.title = flow.name;
      continue;
    }
    // The flow is gone: a change still waiting has nowhere to go.
    editorPages.get(id)!.editor.discard();
    removePage(id);
  }
  renderSessions();
}
// The page stays disabled if the flows cannot be read, so nothing is changed blindly.
pages.flows.load = async () => {
  try {
    await flows.reload();
    showPageError(pages.flows, null);
    enablePage(pages.flows, true);
  } catch (err) {
    showPageError(pages.flows, `Flows could not be read (${errorText(err)}). Close and reopen this tab to try again.`);
  }
};
flowsBox.querySelector<HTMLButtonElement>(".configure")!.onclick = () => openPage("flows");
// A failure is already logged and shown in the Flows box.
flows.reload().catch(() => {});

const historyBox = $<HTMLElement>("#history-box");
const sessionHistory = createHistory({
  box: historyBox,
  list: async (limit) => {
    const [page] = await Promise.all([invoke<HistoryPage>("claude_history", { limit }), readTabSessions()]);
    return page;
  },
  errorText,
  folderLabel: (cwd) => folderLabel(cwd, home),
  openIn: (id) => {
    const s = tabWithSession(id);
    return !s ? null : s === active ? "this tab" : `in ${tabLabel(s)}`;
  },
  onResume: (past) => resumeInNewTab(past),
  pin: async (id, pinned) => {
    try {
      await invoke("claude_session_pin", { id, pinned });
      setNotice("history-pin", null);
      return true;
    } catch (err) {
      console.error("misag: could not pin the session", err);
      setNotice("history-pin", `The session could not be ${pinned ? "pinned" : "unpinned"}: ${errorText(err)}.`);
      return false;
    }
  },
});

/** Resumes a past session in a new Claude tab in its folder, or shows the tab that has it open. */
function resumeInNewTab(past: Past) {
  const s = tabWithSession(past.id);
  if (s) return activate(s);
  openTab({ kind: "claude", claudeSession: past.id }, undefined, past.cwd);
}
const toolboxes = setupToolboxes($<HTMLElement>("#sidebar"), (box) => {
  if (box === historyBox) reloadHistoryIfShown();
});
$<HTMLButtonElement>("#settings .reset-sidebar").onclick = () => {
  toolboxes.reset();
  resetSidebarWidth();
};
historyBox.querySelector<HTMLButtonElement>(".refresh")!.onclick = () => void sessionHistory.reload();

/** Marks again which sessions the active tab and the others have open. */
function showHistory() {
  sessionHistory.redraw();
}

/** The Claude session each tab's hook last reported, by tab id, as last read. */
let reportedSessions: Record<string, string> = {};

/** The failure last logged by `readTabSessions`, so one that lasts is logged once. */
let tabSessionsFailure: string | null = null;

/** Reads which session each tab is in; if that fails, the last answer is kept. */
async function readTabSessions() {
  try {
    reportedSessions = await invoke<Record<string, string>>("claude_tab_sessions");
    tabSessionsFailure = null;
  } catch (err) {
    if (errorText(err) !== tabSessionsFailure) console.error("misag: could not read the tabs' Claude sessions", err);
    tabSessionsFailure = errorText(err);
  }
}

/** The running tab that has Claude session `id` open, if any. */
function tabWithSession(id: string): Session | undefined {
  return sessions.find((s) => {
    if (s.exited || s.closing) return false;
    const open = s.run.kind === "claude" ? (reportedSessions[s.tabId] ?? s.run.claudeSession) : s.claudeOpen;
    return open === id;
  });
}

/** Whether the Session history box can be seen: open, with the sidebar shown, in a visible window. */
const historyShown = () =>
  !historyBox.classList.contains("collapsed") && !document.body.classList.contains("sidebar-collapsed") && !document.hidden;

function reloadHistoryIfShown() {
  if (historyShown()) void sessionHistory.reload();
}
document.addEventListener("visibilitychange", reloadHistoryIfShown);
// The path field is read-only: clicking it (or Enter or Space on it) opens the folder picker.
const workspaceInput = pages.workspace.form.elements.namedItem("workspace") as HTMLInputElement;
const pickWorkspace = () =>
  chooseWorkspace().catch((err) => {
    console.error("misag: could not open the folder picker", err);
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
  let warnedFlows = false;
  await getCurrentWindow().onCloseRequested(async (event) => {
    const flowsSaved = await Promise.all([...editorPages.values()].map(({ editor }) => editor.flush()));
    if (flowsSaved.includes(false) && !warnedFlows) {
      warnedFlows = true;
      event.preventDefault();
      setNotice("flows", "Some flow changes are not saved; their tabs say why. Close the window again to quit without them.", false);
      return;
    }
    try {
      await saveLayout();
      warned = false;
    } catch (err) {
      console.error("misag: could not save tabs", err);
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
    console.error("misag: could not load settings", err);
    setNotice(
      "settings",
      `Settings could not be read (${errorText(err)}), ${pages.workspace.unreadable} Claude tabs start with permission prompts and auto memory off.`,
    );
  }
  showWorkspace();
});
// Async so a failure (e.g. outside Tauri) is logged instead of aborting the UI setup.
watchWindowClose().catch((err) => {
  console.error("misag: could not watch window close", err);
  setNotice("close", "Tabs are saved after each change, but not when the window closes.");
});
const checkStore = () =>
  showStoreStatus().catch((err) => {
    console.error("misag: could not read the store status", err);
    setNotice("store", `Could not check whether tabs are being saved (${errorText(err)}).`);
  });
checkStore();
// Session-hook failures can happen at any time, so they are checked on a timer.
setInterval(checkStore, STORE_POLL_MS);
// Directories and the Claude in terminal tabs change without telling misag, and
// flow runs move on in the Stop hook.
setInterval(pollTabs, TAB_POLL_MS);
setInterval(pollRuns, TAB_POLL_MS);
// `misag flow import` adds flows without telling the app.
setInterval(() => void flows.refresh(), TAB_POLL_MS);
setInterval(reloadHistoryIfShown, HISTORY_POLL_MS);
restore()
  .catch((err) => {
    console.error("misag: restore failed", err);
    canSave = false;
    setNotice("load", "Saved tabs could not be restored, so changes are not saved this session.", false);
  })
  .finally(() => {
    tabsRestored = true;
    showTabRuns();
  });
