import { invoke } from "@tauri-apps/api/core";
import { homeDir } from "@tauri-apps/api/path";
import "@wterm/dom/css";
import { CLAUDE_ICON, fillIcons } from "./icons";
import { folderLabel } from "./paths";
import { createTree, type Listing } from "./tree";
import { createFlows } from "./flows";
import { createRunController } from "./runController";
import { createHistory, type Page as HistoryPage, type Past } from "./history";
import { primeAudio } from "./sounds";
import { setupToolboxes } from "./toolbox";
import { errorText, type StoreReport } from "./ipcTypes";
import { createNotices, type NoticeSink } from "./notices";
import { createPages, enablePage, pageIds, showPageError, staticPage, type Page, type StaticPageId } from "./pages";
import { createSettingsPage } from "./settingsPage";
import { createSidebar } from "./sidebar";
import { createLayoutStore } from "./layoutStore";
import { claudeLive, createTabPoller, TAB_POLL_MS } from "./tabPoller";
import { createTabs } from "./tabs";
import { claudeWorking, waitsInBackground, type Session } from "./tabState";
import { createPathTyping } from "./pathDrops";

/** How often the Session history box, while it can be seen, reads the saved Claude sessions again, as Claude adds to them. */
const HISTORY_POLL_MS = 10_000;
/** How often the backend's saving, session hook, wrapper and CLI shim status is read again. */
const STORE_POLL_MS = 30_000;

/**
 * The running app's signal, aborted by the dispose function `boot` returns (and
 * aborted before the first boot), so `boot` can refuse to run twice on one page.
 * Everything else the app holds belongs to the parts `boot` builds, which get this
 * signal so their timer chains and async continuations stop after dispose.
 */
let running: AbortSignal = AbortSignal.abort();

function $<T extends HTMLElement>(sel: string): T {
  const el = document.querySelector<T>(sel);
  if (!el) throw new Error(`misag: missing element ${sel}`);
  return el;
}

/**
 * Shows what the backend knows about saving, the Claude session hook, the `claude`
 * wrapper for terminal tabs and the `misag` CLI shim; nothing once `signal` aborts.
 */
async function showStoreStatus(notices: NoticeSink, pages: Record<StaticPageId, Page>, signal: AbortSignal) {
  const report = await invoke<StoreReport>("store_status");
  if (signal.aborted) return;
  const { store, hookProblem } = report;
  if (store.state === "recovered") notices.set("store", store.notice);
  else if (store.state === "notSaving") notices.set("store", `Tabs are not being saved: ${store.reason}.`, false);
  else notices.set("store", null);
  for (const id of pageIds) pages[id].form.querySelector<HTMLElement>(".not-saving")!.hidden = store.state !== "notSaving";
  notices.set(
    "hook",
    hookProblem && `Claude session tracking failed (${hookProblem.message}); see ${hookProblem.log}.`,
    true,
    hookProblem ? String(hookProblem.at) : undefined,
  );
  notices.set(
    "wrapper",
    report.wrapperProblem && `Claude sessions started in terminal tabs will not be reopened: ${report.wrapperProblem}.`,
  );
  notices.set("cli", report.cliProblem && `misag flow is not available in tabs: ${report.cliProblem}.`);
}

/**
 * Starts the app on the page loaded from index.html: finds its elements, builds the
 * parts of the app on them and wires them together, starts the timers and restores the
 * saved tabs. Throws if the app is already running in this module instance.
 *
 * Returns dispose, which clears the poll timers and the pending tab save and finish
 * timers, removes the window, document, audio priming and Tauri close and drop
 * listeners (once they are added, if that is still pending), disconnects the
 * ResizeObserver, and aborts the running signal, so pending timer chains and startup
 * continuations stop without touching the page or the backend. It does not close
 * tabs, kill their processes or undo what boot rendered. After dispose, boot may be
 * called again; so it may after a boot that threw, which aborts its signal first.
 *
 * The parts are built one after another, and each reaches the ones built after it
 * through arrow functions, which throw until those exist. So a factory must not call
 * what it is given while it is being built, only later (see factories.test.ts); the
 * Session history box, which reads the sessions at once, is built after all its
 * `list` reaches.
 */
export function boot(): () => void {
  if (!running.aborted) throw new Error("misag: boot called twice");
  const lifetime = new AbortController();
  running = lifetime.signal;
  try {
    return build(lifetime);
  } catch (err) {
    // What it started stops, and listeners added with the signal are removed.
    lifetime.abort();
    throw err;
  }
}

/** Builds and wires the app for `boot`, whose dispose aborts `lifetime`. */
function build(lifetime: AbortController): () => void {
  const { signal } = lifetime;
  const tabsEl = $<HTMLDivElement>("#tabs");
  const termsEl = $<HTMLElement>("#terminals");
  const spawnDialog = $<HTMLDialogElement>("#spawn-dialog");
  const noticeEl = $<HTMLDivElement>("#notice");
  const notices = createNotices(noticeEl);
  const pages: Record<StaticPageId, Page> = {
    settings: staticPage("Settings", "settings", "so Claude tabs start with permission prompts and auto memory off, and settings cannot be changed.", $("#settings")),
    workspace: staticPage(
      "Workspace settings",
      "folder-cog",
      "so the Workspace box shows your home directory and its folder cannot be changed.",
      $("#workspace-settings"),
    ),
    flows: staticPage("Flows", "workflow", "", $("#flows-settings")),
  };

  let home: string | null = null;
  homeDir()
    .then((dir) => {
      if (signal.aborted) return;
      home = dir;
      tabs.list.forEach(tabs.updateTab);
    })
    .catch((err) => console.error("misag: could not find the home directory", err));

  // A context started without a key or click may stay silent, so each one primes it while a sound is chosen.
  for (const area of [$("#tabbar"), $("#content")]) {
    for (const type of ["pointerdown", "keydown"]) {
      area.addEventListener(type, () => {
        const { finishSound } = settings.current();
        if (finishSound && finishSound !== "off") primeAudio();
      }, { capture: true, signal });
    }
  }
  for (const id of pageIds) pages[id].form.onsubmit = (e) => e.preventDefault();
  noticeEl.querySelector("button")!.onclick = notices.dismiss;

  const workspaceBox = $<HTMLElement>("#workspace-box");
  const tree = createTree(workspaceBox.querySelector<HTMLUListElement>(".tree")!, {
    list: (dir) => invoke<Listing>("fs_list", { dir }),
    errorText,
    onPickFile: (path) => pathTyping.insert([path]),
    onOpenClaude: (dir) => openClaudeTab(undefined, dir),
  });
  const settings = createSettingsPage({
    settings: pages.settings,
    workspace: pages.workspace,
    notices,
    tree,
    fitShown: () => tabs.fitShown(),
  });

  const openTerminalTab = (name?: string, cwd?: string) => tabs.open({ kind: "terminal", command: null }, name, cwd);
  const openClaudeTab = (name?: string, cwd?: string) => tabs.open({ kind: "claude", claudeSession: null }, name, cwd);
  /** Asks for the new tab's kind, name and directory. */
  function askTab() {
    const form = spawnDialog.querySelector("form")!;
    form.reset();
    // returnValue survives between openings; Escape would otherwise reuse the last "ok".
    spawnDialog.returnValue = "";
    spawnDialog.onclose = () => {
      if (spawnDialog.returnValue !== "ok") return tabs.focusActive();
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
  $<HTMLButtonElement>("#open-settings").onclick = () => pageTabs.open("settings");
  // A hidden terminal has no size to fit to; it is fitted when shown again. While the
  // sidebar's edge is dragged (the body has the `resizing-sidebar` class, see sidebar.ts),
  // it is fitted once let go, so the PTY is not resized at every move.
  const fitObserver = new ResizeObserver(() => {
    if (!document.body.classList.contains("resizing-sidebar")) tabs.fitShown();
  });
  fitObserver.observe(termsEl);
  signal.addEventListener("abort", () => fitObserver.disconnect());

  const sidebarEl = $<HTMLElement>("#sidebar");
  const sidebar = createSidebar({
    sidebar: sidebarEl,
    resizer: $<HTMLElement>("#sidebar-resizer"),
    signal,
    fitShown: () => tabs.fitShown(),
    focusActive: () => tabs.focusActive(),
  });
  $("#toggle-sidebar").onclick = () => {
    sidebar.setCollapsed(!sidebar.collapsed());
    reloadHistoryIfShown();
    tabs.focusActive();
  };

  workspaceBox.querySelector<HTMLButtonElement>(".refresh")!.onclick = () => tree.reload();
  workspaceBox.querySelector<HTMLButtonElement>(".configure")!.onclick = () => pageTabs.open("workspace");
  const flowsBox = $<HTMLElement>("#flows-box");
  const flows = createFlows({
    box: flowsBox,
    page: pages.flows.el,
    errorText,
    showError: (message) => showPageError(pages.flows, message),
    onOpen: (flow) => pageTabs.openFlowEditor(flow),
    onChange: (stored) => pageTabs.followFlows(stored),
    onRun: (flow) => void runs.start(flow),
    onStop: (run) => void runs.stop(run),
    onContinue: async (run, output) => flows.showBoxError(await runs.proceed(run, output)),
  });
  const runs = createRunController<Session>({
    flows,
    errorText,
    tabs: () => tabs.list,
    active: () => tabs.active(),
    label: (s) => tabs.label(s),
    // Claude waiting on background tasks takes /clear, and they may never end.
    busy: claudeWorking,
    waitsInBackground,
    live: claudeLive,
    write: (s, data) => tabs.write(s, data),
    editors: () => pageTabs.editors(),
    markWaiting: (isWaiting) => tabs.list.forEach((s, i) => tabsEl.children[i]?.classList.toggle("needs-you", isWaiting(s))),
    lockChanged: (_s, locked) => {
      if (!locked) notices.set("flow-typing", null);
    },
  });
  const pageTabs = createPages({
    pages,
    terms: termsEl,
    runs,
    render: () => tabs.render(),
    showActive: () => tabs.showActive(),
    showTerminal: () => tabs.showTerminal(),
    loadSettings: (p) => settings.loadInto(p),
  });
  const tabs = createTabs({
    signal,
    bar: tabsEl,
    terms: termsEl,
    notices,
    runs,
    pages: pageTabs,
    home: () => home,
    checks: () => poller.checks(),
    watchHookReport: (s, since, startNo) => poller.watchHookReport(s, since, startNo),
    scheduleSave: () => layout.schedule(),
    showHistory: () => sessionHistory.redraw(),
  });
  const poller = createTabPoller({
    signal,
    notices,
    tabs: () => tabs.list,
    updateTab: tabs.updateTab,
    showHistory: () => sessionHistory.redraw(),
    scheduleSave: () => layout.schedule(),
    playFinish: settings.playFinish,
  });
  const layout = createLayoutStore({
    signal,
    notices,
    tabs: () => tabs.list,
    active: tabs.active,
    followClaude: poller.followClaude,
    spawn: tabs.spawn,
    activate: tabs.activate,
    flushEditors: () => Promise.all([...pageTabs.editors()].map(([, editor]) => editor.flush())),
  });
  const pathTyping = createPathTyping({
    notices,
    active: tabs.active,
    pageShown: () => pageTabs.shown() !== null,
    isOpen: (s) => tabs.list.includes(s),
    heldBack: tabs.heldBack,
    typed: (s, text) => runs.typed(s, text),
    activate: tabs.activate,
  });

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
  flowsBox.querySelector<HTMLButtonElement>(".configure")!.onclick = () => pageTabs.open("flows");
  // A failure is already shown in the Flows box.
  flows.reload().catch((err) => console.error("misag: could not read the flows at startup", err));

  const historyBox = $<HTMLElement>("#history-box");
  /** Whether the Session history box can be seen: open, with the sidebar shown, in a visible window. */
  const historyShown = () => !historyBox.classList.contains("collapsed") && !sidebar.collapsed() && !document.hidden;
  const reloadHistoryIfShown = () => {
    if (historyShown()) void sessionHistory.reload();
  };
  const sessionHistory = createHistory({
    box: historyBox,
    list: async (limit) => {
      const [page] = await Promise.all([invoke<HistoryPage>("claude_history", { limit }), poller.readTabSessions()]);
      return page;
    },
    errorText,
    folderLabel: (cwd) => folderLabel(cwd, home),
    openIn: (id) => {
      const s = poller.tabWithSession(id);
      return !s ? null : s === tabs.active() ? "this tab" : `in ${tabs.label(s)}`;
    },
    // Resumes a past session in a new Claude tab in its folder, or shows the tab that has it open.
    onResume: (past: Past) => {
      const s = poller.tabWithSession(past.id);
      if (s) return tabs.activate(s);
      tabs.open({ kind: "claude", claudeSession: past.id }, undefined, past.cwd);
    },
    pin: async (id, pinned) => {
      try {
        await invoke("claude_session_pin", { id, pinned });
        notices.set("history-pin", null);
        return true;
      } catch (err) {
        console.error("misag: could not pin the session", err);
        notices.set("history-pin", `The session could not be ${pinned ? "pinned" : "unpinned"}: ${errorText(err)}.`);
        return false;
      }
    },
  });
  const toolboxes = setupToolboxes(sidebarEl, (box) => {
    if (box === historyBox) reloadHistoryIfShown();
  });
  $<HTMLButtonElement>("#settings .reset-sidebar").onclick = () => {
    toolboxes.reset();
    sidebar.resetWidth();
  };
  historyBox.querySelector<HTMLButtonElement>(".refresh")!.onclick = () => void sessionHistory.reload();
  document.addEventListener("visibilitychange", reloadHistoryIfShown, { signal });

  tabs.render();
  notices.render();
  // The tree waits for the stored folder.
  settings.start(signal);
  // Async so a failure (e.g. outside Tauri) is logged instead of aborting the UI setup.
  const unlistenClose = layout.watchWindowClose().catch((err) => {
    console.error("misag: could not watch window close", err);
    if (!signal.aborted) notices.set("close", "Tabs are saved after each change, but not when the window closes.");
  });
  const unlistenDrops = pathTyping.watchDrops().catch((err) => {
    console.error("misag: could not watch file drops", err);
    if (!signal.aborted) notices.set("drop", `Dropping files on the window will not type their paths: ${errorText(err)}.`);
  });
  const checkStore = () =>
    showStoreStatus(notices, pages, signal).catch((err) => {
      console.error("misag: could not read the store status", err);
      if (signal.aborted) return;
      notices.set("store", `Could not check whether tabs are being saved (${errorText(err)}).`);
    });
  checkStore();
  const timers = [
    // Session-hook failures can happen at any time, so they are checked on a timer.
    setInterval(checkStore, STORE_POLL_MS),
    // Directories and the Claude in terminal tabs change without telling misag, and
    // flow runs move on in Claude's hooks.
    setInterval(poller.poll, TAB_POLL_MS),
    setInterval(runs.poll, TAB_POLL_MS),
    // `misag flow import` adds flows without telling the app.
    setInterval(() => void flows.refresh(), TAB_POLL_MS),
    setInterval(reloadHistoryIfShown, HISTORY_POLL_MS),
  ];
  layout
    .restore()
    .catch((err) => {
      console.error("misag: restore failed", err);
      if (signal.aborted) return;
      layout.stopSaving("Saved tabs could not be restored, so changes are not saved this session.");
    })
    .finally(() => {
      if (!signal.aborted) runs.restored();
    });

  return () => {
    lifetime.abort();
    for (const timer of timers) clearInterval(timer);
    layout.dispose();
    tabs.dispose();
    // Also when the listener is added only after this.
    void unlistenClose.then((unlisten) => unlisten?.());
    void unlistenDrops.then((unlisten) => unlisten?.());
  };
}
