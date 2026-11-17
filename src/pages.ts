import { createFlowEditor, type FlowEditor } from "./flowEditor";
import type { Flow } from "./flows";
import { icon, type IconName } from "./icons";
import { errorText } from "./ipcTypes";
import type { RunController, RunTab } from "./runController";

/** A tab that shows a page of the app instead of a terminal. Pages are not saved with the tabs. */
export interface Page {
  title: string;
  icon: IconName;
  /** Shown when the stored settings cannot be read, after "Settings could not be read (why), ". */
  unreadable: string;
  el: HTMLElement;
  form: HTMLFormElement;
  /** Its tab in the tab bar while it is open. */
  tabEl: HTMLElement | null;
  /** Fills the page when its tab opens, enabling its form; by default from the stored settings. */
  load?: () => Promise<void>;
  /** Called whenever the pages are redrawn with this one shown. */
  shown?: () => void;
}

/** The fixed pages, always there whether their tabs are open or not. */
export const pageIds = ["settings", "workspace", "flows"] as const;
export type StaticPageId = (typeof pageIds)[number];
type EditorPageId = `flow:${number}`;
/** A page: one of the fixed pages, or the editor page of an open flow. */
export type PageId = StaticPageId | EditorPageId;
const isEditorPage = (id: PageId): id is EditorPageId => id.startsWith("flow:");

/** The page in `el`, whose form holds its controls. */
export function staticPage(title: string, icon: IconName, unreadable: string, el: HTMLElement): Page {
  return { title, icon, unreadable, el, form: el.querySelector("form")!, tabEl: null };
}

/** Enables or disables every control on the page (each part of it is a fieldset). */
export function enablePage(p: Page, on: boolean) {
  for (const fieldset of p.el.querySelectorAll("fieldset")) fieldset.disabled = !on;
}

/** Shows `message` in the page's error line, or hides the line with null. */
export function showPageError(p: Page, message: string | null) {
  const el = p.form.querySelector<HTMLElement>(".error")!;
  el.hidden = !message;
  el.textContent = message ?? "";
}

export interface PagesOptions {
  pages: Record<StaticPageId, Page>;
  /** Where the editor pages go, beside the terminals. */
  terms: HTMLElement;
  runs: Pick<RunController<RunTab>, "proceed" | "resume" | "show">;
  /** Draws the whole tab bar again (see `Pages.renderTabs`). */
  render(): void;
  /** Marks the visible tab and pane again (see `Pages.markActive`). */
  showActive(): void;
  /** Shows the active terminal again, the shown page having closed. */
  showTerminal(): void;
  /** Fills a page without a `load` of its own from the stored settings, enabling it once read. */
  loadSettings(p: Page): void;
}

export interface Pages {
  /** The page shown instead of a terminal; null when a terminal (or nothing) is shown. */
  shown(): PageId | null;
  /** Notes the page shown (null for a terminal), for the caller to redraw. */
  setShown(id: PageId | null): void;
  /**
   * Opens the page's tab and shows it. A tab that was not open yet runs the page's
   * `load` (by default the stored settings); if that fails, the page shows why.
   */
  open(id: PageId): void;
  /**
   * Closes the page's tab. A flow editor's waiting changes are saved first, and if
   * they cannot be, its tab stays open (shown again) saying why.
   */
  close(id: PageId): void;
  /** Adds the open pages' tabs to the tab bar, after the terminal tabs. */
  renderTabs(bar: HTMLElement): void;
  /** Marks the shown page and its tab, telling it that it is shown. */
  markActive(): void;
  /** Opens the flow's editor tab, creating it the first time. */
  openFlowEditor(flow: Flow): void;
  /** Keeps editor tabs in step with the stored flows: renamed ones are retitled, deleted ones closed. */
  followFlows(stored: Flow[]): void;
  /** The open flow editors, by flow id. */
  editors(): Iterable<[number, FlowEditor]>;
}

/** The app's pages and their tabs: Settings, Workspace settings, Flows, and the open flows' editors. */
export function createPages(opts: PagesOptions): Pages {
  const { pages } = opts;
  /** The open flows' editor pages, created when opened and removed when closed. */
  const editorPages = new Map<EditorPageId, { page: Page; editor: FlowEditor }>();
  /** Pages in the tab bar, in the order opened; their tabs come after the terminal tabs. */
  const openPages: PageId[] = [];
  let shownPage: PageId | null = null;

  function pageOf(id: PageId): Page {
    if (!isEditorPage(id)) return pages[id];
    const editor = editorPages.get(id);
    // Editor pages leave the map only after their tab has closed.
    if (!editor) throw new Error(`misag: no editor page ${id}`);
    return editor.page;
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
      close(id);
    };
    tab.append(mark, name, x);
    tab.onclick = () => open(id);
    tab.onauxclick = (e) => {
      if (e.button === 1) close(id);
    };
    return tab;
  }

  function renderTabs(bar: HTMLElement) {
    for (const id of openPages) {
      pageOf(id).tabEl = renderPageTab(id);
      bar.append(pageOf(id).tabEl!);
    }
  }

  function markActive() {
    for (const id of [...pageIds, ...editorPages.keys()]) {
      pageOf(id).el.classList.toggle("active", id === shownPage);
      pageOf(id).tabEl?.classList.toggle("active", id === shownPage);
    }
    if (shownPage !== null) pageOf(shownPage).shown?.();
  }

  function open(id: PageId) {
    const p = pageOf(id);
    if (!openPages.includes(id)) {
      openPages.push(id);
      // Disabled now, not when the load starts, so nothing can change before it.
      enablePage(p, false);
      if (p.load) {
        p.load().catch((err) => {
          console.error("misag: could not load the page", err);
          showPageError(p, `The page could not be loaded (${errorText(err)}). Close and reopen this tab to try again.`);
        });
      } else opts.loadSettings(p);
    }
    shownPage = id;
    opts.render();
    opts.showActive();
    // The page, not a control: a key typed while switching must not change a setting.
    p.el.focus();
  }

  function close(id: PageId) {
    const editor = isEditorPage(id) ? editorPages.get(id) : undefined;
    if (!editor) return remove(id);
    editor.editor
      .flush()
      .then((saved) => {
        if (saved) remove(id);
        else if (openPages.includes(id)) open(id);
      })
      .catch((err) => {
        console.error("misag: could not save the flow's changes", err);
        if (!openPages.includes(id)) return;
        showPageError(editor.page, `The flow's changes could not be saved (${errorText(err)}), so its tab stays open.`);
        open(id);
      });
  }

  /** Closes the page's tab right away; a flow editor's page goes with it. */
  function remove(id: PageId) {
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
    opts.render();
    if (wasShown) opts.showTerminal();
    else opts.showActive();
  }

  function openFlowEditor(flow: Flow) {
    const id: EditorPageId = `flow:${flow.id}`;
    if (!editorPages.has(id)) {
      const editor = createFlowEditor(flow.id, { errorText, onContinue: opts.runs.proceed, onResume: opts.runs.resume });
      opts.terms.append(editor.el);
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
      opts.runs.show();
    }
    open(id);
  }

  function followFlows(stored: Flow[]) {
    for (const [id, { page }] of editorPages) {
      const flow = stored.find((f) => `flow:${f.id}` === id);
      if (flow) {
        page.title = flow.name;
        continue;
      }
      // The flow is gone: a change still waiting has nowhere to go.
      editorPages.get(id)!.editor.discard();
      remove(id);
    }
    opts.render();
  }

  function* editors(): Iterable<[number, FlowEditor]> {
    for (const [id, { editor }] of editorPages) yield [Number(id.slice("flow:".length)), editor];
  }

  return {
    shown: () => shownPage,
    setShown: (id) => void (shownPage = id),
    open,
    close,
    renderTabs,
    markActive,
    openFlowEditor,
    followFlows,
    editors,
  };
}
