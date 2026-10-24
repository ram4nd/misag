import { icon } from "./icons";

/** Mirrors `history::Past` in src-tauri/src/history.rs. */
export interface Past {
  id: string;
  title: string;
  /** The folder Claude ran in. */
  cwd: string;
  /** When the transcript last changed, in unix milliseconds. */
  modified: number;
}

/** Mirrors `history::Page` in src-tauri/src/history.rs. */
export interface Page {
  sessions: Past[];
  /** There are older ones. */
  more: boolean;
  /** Transcripts left out because they could not be read. */
  unreadable: number;
}

/** Sessions shown at first, and added by each Load more. */
const PAGE_SIZE = 10;

export interface HistoryOptions {
  /** The Session history toolbox in the sidebar. */
  box: HTMLElement;
  /** Lists the newest `limit` Claude sessions saved in a folder. */
  list(dir: string, limit: number): Promise<Page>;
  errorText(err: unknown): string;
  /** Where the session is open ("this tab", "in work"), or null when no tab has it open. */
  openIn(id: string): string | null;
  /** Resumes the session in the active tab (a click on its row). */
  onResume(past: Past): void;
  /** Resumes the session in a new Claude tab (its new-tab button). */
  onOpenTab(past: Past): void;
}

export interface History {
  /** Shows the sessions of `dir` (null: no tab), reading them if the folder changed. */
  show(dir: string | null): void;
  /** Reads the shown folder's sessions again. */
  reload(): Promise<void>;
}

const NONE: Page = { sessions: [], more: false, unreadable: 0 };

/** "now" under a minute, "5m ago", "3h ago", "2d ago", else (from 30 days) the date. */
function age(ms: number, now = Date.now()): string {
  const minutes = Math.floor((now - ms) / 60_000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m ago`;
  if (minutes < 24 * 60) return `${Math.floor(minutes / 60)}h ago`;
  if (minutes < 30 * 24 * 60) return `${Math.floor(minutes / (24 * 60))}d ago`;
  return new Date(ms).toLocaleDateString();
}

/** The Session history toolbox: the Claude sessions saved in the active tab's folder. */
export function createHistory(opts: HistoryOptions): History {
  const list = opts.box.querySelector<HTMLUListElement>(".sessions")!;
  const hint = opts.box.querySelector<HTMLElement>(".hint")!;
  const error = opts.box.querySelector<HTMLElement>(".error")!;
  const loadMore = opts.box.querySelector<HTMLButtonElement>(".load-more")!;
  const unreadable = opts.box.querySelector<HTMLElement>(".unreadable")!;
  /** The last page read for the shown folder, kept on screen when a later read fails. */
  let page = NONE;
  /** What is on screen, so a read that changes nothing leaves the rows (and the focus in them) alone. */
  let rendered = "";
  let dir: string | null = null;
  /** How many sessions are shown; Load more adds a page. */
  let limit = PAGE_SIZE;
  /** Counts reads, so an answer for a folder left meanwhile is dropped. */
  let reads = 0;

  function row(past: Past): HTMLLIElement {
    const li = document.createElement("li");
    const open = opts.openIn(past.id);
    const resume = document.createElement("button");
    resume.type = "button";
    resume.className = "resume";
    const action = open ? `Open ${open}: click to show it` : "Resume in the active tab";
    resume.title = `${past.title}\n${new Date(past.modified).toLocaleString()}\n${action}`;
    const title = document.createElement("span");
    title.className = "title";
    title.textContent = past.title;
    const when = document.createElement("span");
    when.className = "age";
    when.textContent = age(past.modified);
    resume.append(title);
    if (open) resume.append(Object.assign(document.createElement("span"), { className: "open", textContent: open }));
    resume.append(when);
    resume.onclick = () => opts.onResume(past);
    const newTab = document.createElement("button");
    newTab.type = "button";
    newTab.className = "new-tab";
    newTab.title = open ? `Open ${open}: show it` : "Resume in a new Claude tab";
    newTab.append(icon("square-arrow-out-up-right", 14));
    newTab.onclick = () => opts.onOpenTab(past);
    li.append(resume, newTab);
    return li;
  }

  function render(shown: Page, message: string | null) {
    const { sessions, more } = shown;
    const state = JSON.stringify([dir, shown, message, sessions.map((p) => opts.openIn(p.id))]);
    if (state === rendered) return;
    rendered = state;
    list.replaceChildren(...sessions.map(row));
    loadMore.hidden = !more;
    unreadable.hidden = !shown.unreadable;
    unreadable.textContent = `${shown.unreadable} session${shown.unreadable === 1 ? "" : "s"} could not be read; raterm's log says why.`;
    error.hidden = !message;
    error.textContent = message ?? "";
    hint.hidden = sessions.length > 0 || !error.hidden;
    hint.textContent = dir === null ? "Select a tab to see the Claude sessions of its folder." : "No Claude sessions in this folder yet.";
  }

  /** Reads the shown folder's sessions; false if that failed (the last ones read stay shown). */
  async function reload(): Promise<boolean> {
    const read = ++reads;
    if (dir === null) {
      render(NONE, null);
      return true;
    }
    const shown = dir;
    try {
      const next = await opts.list(shown, limit);
      if (read === reads) render((page = next), null);
      return true;
    } catch (err) {
      console.error("raterm: could not read the Claude sessions of", shown, err);
      const message = `Sessions could not be read: ${opts.errorText(err)}. The reload button tries again.`;
      if (read === reads) render(page, message);
      return false;
    }
  }

  loadMore.onclick = async () => {
    limit += PAGE_SIZE;
    // A failed read asks for the same page next time.
    if (!(await reload())) limit = Math.max(PAGE_SIZE, limit - PAGE_SIZE);
  };
  render(NONE, null);
  return {
    show(next) {
      if (next === dir) return;
      dir = next;
      limit = PAGE_SIZE;
      page = NONE;
      render(NONE, null);
      void reload();
    },
    reload: async () => {
      await reload();
    },
  };
}
