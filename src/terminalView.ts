import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { WTerm } from "@wterm/dom";
import { errorText } from "./ipcTypes";
import type { NoticeSink } from "./notices";

/**
 * A terminal view in a new pane in `terms`; its colors and font are set in style.css. It
 * paints only while shown, and is sized by the app's own observer, not wterm's.
 */
export function createView(terms: HTMLElement): { el: HTMLDivElement; term: WTerm } {
  const el = document.createElement("div");
  el.className = "term";
  terms.append(el);
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

export interface MouseOptions {
  /** The running app's signal: a pending focus, copy or paste is dropped once it aborts. */
  signal: AbortSignal;
  /** Whether the terminal's pane is the one shown, not hidden or under a page. */
  shown(): boolean;
  notices: NoticeSink;
}

/** Sets up the terminal's middle click, left button release and right click, once it has loaded. */
export function handleMouse(term: WTerm, opts: MouseOptions) {
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
    void pasteFrom(term, "primary_selection", opts);
  });
  // wterm focuses only on a click, which WebKit does not fire when Claude redrew the
  // pressed row between press and release. So letting go of the left button anywhere on
  // the shown pane focuses it, unless that selected text, which is copied instead (as
  // most terminals do) and stays selected. A hidden pane, or one under a page, does
  // neither, so it cannot put stale text on the clipboard.
  term.element.addEventListener("mouseup", (e) => {
    if (e.button !== 0) return;
    // After wterm's own click handling, which may have selected a word.
    setTimeout(() => {
      if (opts.signal.aborted || !opts.shown()) return;
      if (term.getSelectionText?.()) {
        // wterm's copy event listener puts its selection on the clipboard.
        if (!document.execCommand("copy")) console.warn("misag: the selection could not be copied");
        return;
      }
      if (getSelection()?.isCollapsed ?? true) term.focus();
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
    void pasteFrom(term, "clipboard_text", opts);
  });
}

/** What a terminal can paste: the backend command that reads it, and its name. */
const PASTE_SOURCES = { primary_selection: "primary selection", clipboard_text: "clipboard" } as const;

/**
 * Types the primary selection (what is selected anywhere on the desktop) or the
 * clipboard into the terminal: in brackets if its program asked for them, with line
 * breaks as Enter sends them and no control characters but Tab, so the text cannot
 * end the brackets early or send keys of its own. Without brackets each line runs as
 * it arrives, so more than one line is pasted only if the person agrees. If the text
 * cannot be read or the question cannot be asked, a notice says so; after the
 * running app's signal aborts, nothing is typed.
 */
async function pasteFrom(term: WTerm, source: keyof typeof PASTE_SOURCES, opts: Pick<MouseOptions, "signal" | "notices">) {
  const { signal, notices } = opts;
  const what = PASTE_SOURCES[source];
  let text: string | null;
  try {
    text = await invoke<string | null>(source);
  } catch (err) {
    console.error(`misag: could not read the ${what}`, err);
    if (!signal.aborted) notices.set("paste", `Could not paste the ${what}: ${errorText(err)}.`);
    return;
  }
  if (signal.aborted) return;
  notices.set("paste", null);
  if (!text) return;
  const clean = text.replace(/\r\n?|\n/g, "\r").replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/g, "");
  const bracketed = term.bridge?.bracketedPaste() ?? false;
  if (!bracketed && clean.includes("\r")) {
    let sure: boolean;
    try {
      sure = await ask("The text has more than one line, and the program in this tab runs each line as it is pasted. Paste it?", { title: "Paste", kind: "warning" });
    } catch (err) {
      console.error(`misag: could not ask before pasting the ${what}`, err);
      if (!signal.aborted) notices.set("paste", `Could not paste the ${what}: ${errorText(err)}.`);
      return;
    }
    if (signal.aborted) return;
    term.focus();
    if (!sure) return;
  }
  term.onData?.(bracketed ? `\x1b[200~${clean}\x1b[201~` : clean);
}
