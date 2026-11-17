import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { errorText } from "./ipcTypes";
import type { NoticeSink } from "./notices";
import { notLocal, shellQuote, untypeable } from "./paths";
import type { Session } from "./tabState";

/** The most files one drop types; more would flood the tab's prompt. */
const MAX_DROPPED = 100;

/** `paths` as a notice names them: the first, and how many more. */
function namePaths(paths: string[]): string {
  const first = JSON.stringify(paths[0]);
  return paths.length > 1 ? `${first} and ${paths.length - 1} more` : first;
}

export interface PathTypingOptions {
  notices: NoticeSink;
  /** The terminal tab selected last. */
  active(): Session | null;
  /** Whether a page is shown instead of a terminal. */
  pageShown(): boolean;
  /** Whether the tab is still open. */
  isOpen(s: Session): boolean;
  /** Whether typing into the tab is held back while a flow types there (and says so). */
  heldBack(s: Session): boolean;
  /** Notes what is typed into the tab, for its flow run. */
  typed(s: Session, text: string): void;
  activate(s: Session): void;
}

export interface PathTyping {
  /**
   * Types the shell-quoted `paths`, each followed by a space, into the active tab, as if
   * the user had (as a bracketed paste if its program asked for one), and shows the tab.
   * Types none of them and says why in a notice when there is no running tab or a path
   * cannot be typed safely, and says so when the write fails.
   */
  insert(paths: string[]): void;
  /**
   * Watches for files dragged in from a file manager, which reach the app as paths, not
   * as an HTML drop. Resolves to the function that stops watching.
   */
  watchDrops(): Promise<() => void>;
}

/** Types file paths, picked in the Workspace box or dropped on the window, into the active tab. */
export function createPathTyping(opts: PathTypingOptions): PathTyping {
  const { notices } = opts;
  const encoder = new TextEncoder();

  /** Says why none of `paths` was typed. */
  function refusePaths(paths: string[], why: string) {
    notices.set("path", `${namePaths(paths)} ${paths.length > 1 ? "were" : "was"} not typed: ${why}.`);
  }

  /** Says why `path`, one of `paths`, cannot be typed, and so none of them was. */
  function refusePath(paths: string[], path: string, why: string) {
    const rest = paths.length > 1 ? `, so none of the ${paths.length} dropped files were` : "";
    notices.set("path", `${JSON.stringify(path)} was not typed: ${why}${rest}.`);
  }

  function insert(paths: string[]) {
    const s = opts.active();
    if (!s || s.exited || s.id === null) return refusePaths(paths, "no tab is running");
    if (opts.heldBack(s)) return;
    for (const path of paths) {
      const why = untypeable(path);
      if (why) return refusePath(paths, path, why);
    }
    const text = paths.map((p) => `${shellQuote(p)} `).join("");
    opts.typed(s, text);
    const bracketed = s.term.bridge?.bracketedPaste() ?? false;
    invoke("pty_write", { id: s.id, data: Array.from(encoder.encode(bracketed ? `\x1b[200~${text}\x1b[201~` : text)) })
      .then(() => {
        notices.set("path", null);
        if (opts.isOpen(s)) opts.activate(s);
      })
      .catch((err) => {
        console.error("misag: could not type the paths", s.id, err);
        notices.set("path", `${namePaths(paths)} could not be typed: ${errorText(err)}.`);
      });
  }

  /**
   * Types the paths of files dropped on the window into the active tab, and brings the
   * window to the front so the keys typed next go there, not to the file manager. A drop
   * on a page, of more than MAX_DROPPED files, or of anything but local files types none.
   */
  function drop(paths: string[]) {
    if (!paths.length) return;
    getCurrentWindow()
      .setFocus()
      .catch((err) => console.error("misag: could not bring the window to the front", err));
    if (opts.pageShown()) return notices.set("path", "Drop files on a terminal tab to type their paths.");
    if (paths.length > MAX_DROPPED) return refusePaths(paths, `more than ${MAX_DROPPED} files were dropped`);
    for (const path of paths) {
      const why = notLocal(path);
      if (why) return refusePath(paths, path, why);
    }
    insert(paths);
  }

  function watchDrops(): Promise<() => void> {
    return getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "drop") drop(e.payload.paths);
    });
  }

  return { insert, watchDrops };
}
