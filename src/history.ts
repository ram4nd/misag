import { icon } from "./icons";

/** Mirrors `history::Past` in src-tauri/src/history.rs. */
export interface Past {
  id: string;
  title: string;
  /** The folder Claude ran in. */
  cwd: string;
  /** When the transcript last changed, in unix milliseconds. */
  modified: number;
  /** Pinned to the top of the box. */
  pinned: boolean;
}

/** Mirrors `history::Page` in src-tauri/src/history.rs. */
export interface Page {
  sessions: Past[];
  /** There are older ones. */
  more: boolean;
  /** Transcripts and folders left out because they could not be read. */
  unreadable: number;
}

/** Sessions shown at first, and added by each Load more. */
const PAGE_SIZE = 10;

export interface HistoryOptions {
  /** The Session history toolbox in the sidebar. */
  box: HTMLElement;
  /** Lists the newest `limit` Claude sessions saved in any folder. */
  list(limit: number): Promise<Page>;
  errorText(err: unknown): string;
  /** How a row names the session's folder. */
  folderLabel(cwd: string): string;
  /** Where the session is open ("this tab", "in work"), or null when no tab has it open. */
  openIn(id: string): string | null;
  /** Resumes the session in a new Claude tab (a click on its row). */
  onResume(past: Past): void;
  /**
   * Pins the session to the top of the box, or unpins it (its pin button): true
   * once done, false (after showing why) if that failed.
   */
  pin(id: string, pinned: boolean): Promise<boolean>;
}

export interface History {
  /** Draws the rows again with what was last read, for a change in which tabs have them open. */
  redraw(): void;
  /** Reads the sessions again. */
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

/** The Session history toolbox: the Claude sessions saved in every folder, newest first. */
export function createHistory(opts: HistoryOptions): History {
  const list = opts.box.querySelector<HTMLUListElement>(".sessions")!;
  const hint = opts.box.querySelector<HTMLElement>(".hint")!;
  const error = opts.box.querySelector<HTMLElement>(".error")!;
  const loadMore = opts.box.querySelector<HTMLButtonElement>(".load-more")!;
  const unreadable = opts.box.querySelector<HTMLElement>(".unreadable")!;
  /** The last page read, kept on screen when a later read fails. */
  let page = NONE;
  /** Why the last read failed, if it did. */
  let failure: string | null = null;
  /** What is on screen, so a read that changes nothing leaves the rows (and the focus in them) alone. */
  let rendered = "";
  /** How many sessions are shown; Load more adds a page. */
  let limit = PAGE_SIZE;
  /** Counts reads, so an answer overtaken by a later read is dropped. */
  let reads = 0;

  function row(past: Past): HTMLLIElement {
    const li = document.createElement("li");
    const open = opts.openIn(past.id);
    const resume = document.createElement("button");
    resume.type = "button";
    resume.className = "resume";
    const action = open ? `Open ${open}: click to show it` : "Click to resume it in a new Claude tab";
    resume.title = `${past.title}\n${past.cwd}\n${new Date(past.modified).toLocaleString()}\n${action}`;
    const span = (className: string, textContent: string) => Object.assign(document.createElement("span"), { className, textContent });
    const meta = span("meta", "");
    meta.append(span("folder", opts.folderLabel(past.cwd)));
    if (open) meta.append(span("open", open));
    meta.append(span("age", age(past.modified)));
    resume.append(span("title", past.title), meta);
    resume.onclick = () => opts.onResume(past);
    const pin = document.createElement("button");
    pin.type = "button";
    pin.className = "pin";
    pin.title = past.pinned ? "Unpin" : "Pin to the top";
    pin.setAttribute("aria-pressed", String(past.pinned));
    pin.append(icon("pin", 14));
    pin.onclick = async () => {
      // One request at a time; after a failure the row stays as it was.
      pin.disabled = true;
      let done = false;
      try {
        done = await opts.pin(past.id, !past.pinned);
      } finally {
        pin.disabled = false;
      }
      if (done) await reload();
    };
    li.classList.toggle("pinned", past.pinned);
    li.append(resume, pin);
    return li;
  }

  function render(shown: Page, message: string | null) {
    const { sessions, more } = shown;
    const state = JSON.stringify([shown, message, sessions.map((p) => [opts.openIn(p.id), opts.folderLabel(p.cwd)])]);
    if (state === rendered) return;
    rendered = state;
    list.replaceChildren(...sessions.map(row));
    loadMore.hidden = !more;
    unreadable.hidden = !shown.unreadable;
    unreadable.textContent = `${shown.unreadable} session${shown.unreadable === 1 ? "" : "s"} could not be read; started from a terminal, Mischief of Agents prints why.`;
    error.hidden = !message;
    error.textContent = message ?? "";
    hint.hidden = sessions.length > 0 || !error.hidden;
    hint.textContent = "No Claude sessions saved yet.";
  }

  /** The failure last logged, so one that lasts is not logged on every poll. */
  let logged: string | null = null;

  /** Reads the sessions; false if that failed (the last ones read stay shown). */
  async function reload(): Promise<boolean> {
    const read = ++reads;
    try {
      const next = await opts.list(limit);
      if (read !== reads) return true;
      logged = null;
      render((page = next), (failure = null));
      return true;
    } catch (err) {
      const message = `Sessions could not be read: ${opts.errorText(err)}. The reload button tries again.`;
      if (read !== reads) return false;
      if (message !== logged) console.error("misag: could not read the Claude sessions", err);
      logged = message;
      render(page, (failure = message));
      return false;
    }
  }

  loadMore.onclick = async () => {
    // One page at a time, so a failed read takes back only its own.
    loadMore.disabled = true;
    limit += PAGE_SIZE;
    try {
      // A failed read asks for the same page next time.
      if (!(await reload())) limit = Math.max(PAGE_SIZE, limit - PAGE_SIZE);
    } finally {
      loadMore.disabled = false;
    }
  };
  render(NONE, null);
  void reload();
  return {
    redraw: () => render(page, failure),
    reload: async () => {
      await reload();
    },
  };
}
