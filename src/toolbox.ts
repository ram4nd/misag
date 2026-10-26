/** Remembers an on/off UI choice. Storage can be unavailable; these are only conveniences. */
export function remember(key: string, on: boolean) {
  try {
    localStorage.setItem(key, on ? "1" : "0");
  } catch (err) {
    console.debug("misag: could not remember", key, err);
  }
}

export function remembered(key: string): boolean {
  try {
    return localStorage.getItem(key) === "1";
  } catch (err) {
    console.debug("misag: could not recall", key, err);
    return false;
  }
}

export function forget(key: string) {
  try {
    localStorage.removeItem(key);
  } catch (err) {
    console.debug("misag: could not forget", key, err);
  }
}

/** The toolboxes' order, as their ids, in localStorage. */
const ORDER_KEY = "misag.toolboxOrder";
/** How far (px) the mouse must move with a header (or a tab) held down before it drags it. */
export const DRAG_THRESHOLD = 4;
/** How long toolboxes take to slide into place while one is dragged, and the dragged one after it is dropped. */
const SLIDE_MS = 180;
const EASING = "cubic-bezier(.2, 0, 0, 1)";
/** A click this soon (ms) after a drag ends is the one that ends it, not a click on the header. */
const DRAG_CLICK_MS = 300;

const reducedMotion = () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;

/**
 * Slides `el` into its place from `from` px off it (positive: below, or right along X). Does
 * nothing when `from` is 0, without animation support, or when reduced motion is asked for.
 */
export function slide(el: HTMLElement, from: number, axis: "X" | "Y" = "Y"): Animation | undefined {
  if (!from || !el.animate || reducedMotion()) return undefined;
  return el.animate([{ transform: `translate${axis}(${from}px)` }, { transform: "none" }], { duration: SLIDE_MS, easing: EASING });
}

const collapsedKey = (box: HTMLElement) => `misag.toolbox.${box.id.replace(/-box$/, "")}.collapsed`;

export interface Toolboxes {
  /** Puts the toolboxes back in their first order, all open, and forgets the remembered layout. */
  reset(): void;
}

/**
 * Wires the sidebar's toolboxes: each header's toggle collapses its toolbox
 * (remembered per toolbox), and toolboxes are reordered by dragging their header,
 * or with Alt+Up and Alt+Down on it (remembered too). `onToggle` is called after a
 * toolbox opens or closes.
 */
export function setupToolboxes(sidebar: HTMLElement, onToggle: (box: HTMLElement) => void): Toolboxes {
  const boxes = () => [...sidebar.querySelectorAll<HTMLElement>(":scope > .toolbox")];
  const initial = boxes();
  /** A toolbox's place in the sidebar (px from its top), ignoring transforms and animations. */
  const place = (b: HTMLElement) => b.offsetTop;
  /** The toolbox being dragged, if any. */
  let dragged: HTMLElement | null = null;
  /** Until when (Date.now() ms) a click on a header ends a drag rather than toggling. */
  let swallowUntil = 0;

  function saveOrder() {
    try {
      localStorage.setItem(ORDER_KEY, JSON.stringify(boxes().map((b) => b.id)));
    } catch (err) {
      console.debug("misag: could not remember the toolbox order", err);
    }
  }

  /** Puts the toolboxes in `order`, followed by any it leaves out. */
  function arrange(order: HTMLElement[]) {
    const all = boxes();
    const end = all.at(-1)!.nextSibling;
    for (const box of [...order, ...all.filter((b) => !order.includes(b))]) sidebar.insertBefore(box, end);
  }

  /** The remembered order; unknown ids are ignored. */
  function restoreOrder() {
    let ids: unknown;
    try {
      ids = JSON.parse(localStorage.getItem(ORDER_KEY) ?? "[]");
    } catch (err) {
      console.debug("misag: could not recall the toolbox order", err);
      return;
    }
    if (!Array.isArray(ids)) return;
    arrange(ids.map((id) => initial.find((b) => b.id === id)).filter((b): b is HTMLElement => !!b));
  }

  /** Moves `box` before `ref` (null: last), sliding the toolboxes it passes into their new places. */
  function reorder(box: HTMLElement, ref: Element | null) {
    const others = boxes().filter((b) => b !== box);
    const before = new Map(others.map((b) => [b, b.getBoundingClientRect().top]));
    for (const b of others) for (const a of b.getAnimations?.() ?? []) a.cancel();
    if (ref) ref.before(box);
    else others.at(-1)!.after(box);
    for (const b of others) slide(b, before.get(b)! - b.getBoundingClientRect().top);
  }

  /** The next toolbox after `box`, or null. */
  const nextBox = (box: Element) => (box.nextElementSibling?.classList.contains("toolbox") ? box.nextElementSibling : null);
  const prevBox = (box: Element) => (box.previousElementSibling?.classList.contains("toolbox") ? box.previousElementSibling : null);

  function startDrag(box: HTMLElement, down: MouseEvent) {
    const startY = down.clientY;
    /** Where in the toolbox it was grabbed, so it stays under the mouse at that point. */
    const grab = down.clientY - sidebar.getBoundingClientRect().top + sidebar.scrollTop - place(box);
    let dragging = false;
    /** How far the dragged toolbox is drawn off its place, to follow the mouse. */
    let shift = 0;

    const move = (e: MouseEvent) => {
      if (!dragging && Math.abs(e.clientY - startY) < DRAG_THRESHOLD) return;
      if (!dragging) {
        dragging = true;
        dragged = box;
        for (const a of box.getAnimations?.() ?? []) a.cancel();
        box.classList.add("dragging");
        document.body.classList.add("dragging-toolbox");
      }
      e.preventDefault();
      const top = e.clientY - sidebar.getBoundingClientRect().top + sidebar.scrollTop - grab;
      const bottom = top + box.offsetHeight;
      for (;;) {
        const prev = prevBox(box) as HTMLElement | null;
        const next = nextBox(box) as HTMLElement | null;
        if (prev && top < place(prev) + prev.offsetHeight / 2) reorder(box, prev);
        else if (next && bottom > place(next) + next.offsetHeight / 2) reorder(box, nextBox(next));
        else break;
      }
      shift = top - place(box);
      box.style.transform = `translateY(${shift}px)`;
    };

    const up = () => {
      document.removeEventListener("mousemove", move);
      document.removeEventListener("mouseup", up);
      if (!dragging) return;
      dragged = null;
      // Settles into its place from where it was let go.
      box.style.transform = "";
      document.body.classList.remove("dragging-toolbox");
      const settle = slide(box, shift);
      // Not when it was grabbed again meanwhile, which cancels this animation.
      const done = () => dragged !== box && box.classList.remove("dragging");
      if (settle) settle.onfinish = settle.oncancel = done;
      else done();
      saveOrder();
      swallowUntil = Date.now() + DRAG_CLICK_MS;
    };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
  }

  restoreOrder();
  for (const box of initial) {
    const header = box.querySelector("header")!;
    const toggle = header.querySelector<HTMLButtonElement>(".toggle")!;
    box.classList.toggle("collapsed", remembered(collapsedKey(box)));
    toggle.onclick = () => {
      const collapsed = !box.classList.contains("collapsed");
      box.classList.toggle("collapsed", collapsed);
      remember(collapsedKey(box), collapsed);
      onToggle(box);
    };
    toggle.addEventListener("keydown", (e) => {
      if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
      e.preventDefault();
      const neighbour = e.key === "ArrowUp" ? prevBox(box) : nextBox(box);
      if (!neighbour) return;
      reorder(box, e.key === "ArrowUp" ? neighbour : nextBox(neighbour));
      saveOrder();
      toggle.focus();
    });
    header.addEventListener("mousedown", (down) => {
      if (down.button === 0) startDrag(box, down);
    });
  }
  // The click that ends a drag is not a click on the header's buttons. Only one
  // that comes right away counts: a drag released outside the window sends none.
  sidebar.addEventListener(
    "click",
    (e) => {
      if (Date.now() > swallowUntil) return;
      swallowUntil = 0;
      e.stopPropagation();
      e.preventDefault();
    },
    { capture: true },
  );

  return {
    reset() {
      forget(ORDER_KEY);
      arrange(initial);
      for (const box of initial) {
        forget(collapsedKey(box));
        if (box.classList.contains("collapsed")) {
          box.classList.remove("collapsed");
          onToggle(box);
        }
      }
    },
  };
}
