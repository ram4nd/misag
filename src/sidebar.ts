import { forget, remember, remembered } from "./toolbox";

/** Whether the sidebar is collapsed, in localStorage. */
const SIDEBAR_KEY = "misag.sidebarCollapsed";
/** The sidebar's width (px) in localStorage, and its default and limits. */
const SIDEBAR_WIDTH_KEY = "misag.sidebarWidth";
const SIDEBAR_WIDTH = 240;
const SIDEBAR_MIN = 160;
const SIDEBAR_MAX = 600;
/** The narrowest (px) the sidebar may squeeze the terminals to. */
const TERMINALS_MIN = 320;

export interface SidebarOptions {
  sidebar: HTMLElement;
  /** The sidebar's edge, dragged (or moved with the keyboard) to resize it. */
  resizer: HTMLElement;
  /** Removes the window listener when aborted. */
  signal: AbortSignal;
  /** Fits the shown terminal, once the edge is let go. */
  fitShown(): void;
  /** Gives the keyboard back to the active terminal. */
  focusActive(): void;
}

export interface Sidebar {
  setCollapsed(collapsed: boolean): void;
  collapsed(): boolean;
  /** Puts the sidebar back at its default width and forgets the one chosen. */
  resetWidth(): void;
}

/** Shows the sidebar collapsed or not and at the width remembered, and lets its edge be moved. */
export function createSidebar(opts: SidebarOptions): Sidebar {
  const { sidebar, resizer } = opts;
  /** The width chosen for the sidebar; a narrow window shows it narrower, until the window grows again. */
  let chosenWidth = SIDEBAR_WIDTH;
  /** The width the sidebar is shown at. */
  let width = SIDEBAR_WIDTH;

  function setCollapsed(collapsed: boolean) {
    document.body.classList.toggle("sidebar-collapsed", collapsed);
    remember(SIDEBAR_KEY, collapsed);
  }

  /** The widest the sidebar may be in this window. */
  const widest = () => Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, window.innerWidth - TERMINALS_MIN));

  /** Shows the sidebar at `px`, kept within its limits. */
  function showWidth(px: number) {
    const max = widest();
    width = Math.round(Math.min(max, Math.max(SIDEBAR_MIN, px)));
    sidebar.style.setProperty("--sidebar-width", `${width}px`);
    resizer.setAttribute("aria-valuenow", String(width));
    resizer.setAttribute("aria-valuetext", `${width} pixels`);
    resizer.setAttribute("aria-valuemax", String(max));
  }

  /** Sets the sidebar's width and remembers it. */
  function chooseWidth(px: number) {
    showWidth(px);
    chosenWidth = width;
    try {
      localStorage.setItem(SIDEBAR_WIDTH_KEY, String(width));
    } catch (err) {
      console.debug("misag: could not remember the sidebar width", err);
    }
  }

  function resetWidth() {
    forget(SIDEBAR_WIDTH_KEY);
    chosenWidth = SIDEBAR_WIDTH;
    showWidth(SIDEBAR_WIDTH);
  }

  /** Shows the sidebar at the width remembered, or its default. */
  function recallWidth() {
    let stored: string | null = null;
    try {
      stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    } catch (err) {
      console.debug("misag: could not recall the sidebar width", err);
    }
    const px = stored ? Number(stored) : NaN;
    chosenWidth = Number.isFinite(px) ? Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, px)) : SIDEBAR_WIDTH;
    showWidth(chosenWidth);
  }

  /** Drags the sidebar's edge, from a mousedown on it. */
  function dragEdge(down: MouseEvent) {
    if (down.button !== 0) return;
    down.preventDefault();
    const start = width;
    // While the body has the `resizing-sidebar` class, the terminals are not fitted at
    // every move (see the ResizeObserver in main.ts); they are once it is let go.
    document.body.classList.add("resizing-sidebar");
    const move = (e: MouseEvent) => {
      // Let go outside the window, where the mouseup went elsewhere.
      if (!(e.buttons & 1)) return end();
      showWidth(start + e.clientX - down.clientX);
    };
    const end = () => {
      document.removeEventListener("mousemove", move);
      document.removeEventListener("mouseup", up);
      window.removeEventListener("blur", end);
      document.body.classList.remove("resizing-sidebar");
      chooseWidth(width);
      opts.fitShown();
      opts.focusActive();
    };
    const up = (e: MouseEvent) => {
      showWidth(start + e.clientX - down.clientX);
      end();
    };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
    window.addEventListener("blur", end);
  }

  /** Left and Right move the edge 16px; Home and End take it to the narrowest and widest. */
  const keys: Record<string, () => number> = {
    ArrowLeft: () => width - 16,
    ArrowRight: () => width + 16,
    Home: () => SIDEBAR_MIN,
    End: widest,
  };

  setCollapsed(remembered(SIDEBAR_KEY));
  resizer.setAttribute("aria-valuemin", String(SIDEBAR_MIN));
  recallWidth();
  window.addEventListener("resize", () => showWidth(chosenWidth), { signal: opts.signal });
  resizer.onmousedown = dragEdge;
  resizer.onkeydown = (e) => {
    const to = keys[e.key];
    if (!to) return;
    e.preventDefault();
    chooseWidth(to());
  };
  resizer.ondblclick = resetWidth;

  return { setCollapsed, collapsed: () => document.body.classList.contains("sidebar-collapsed"), resetWidth };
}
