import {
  Archive,
  ArchiveRestore,
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronRight,
  createElement,
  File,
  Folder,
  FolderCog,
  FolderOpen,
  House,
  Menu,
  Pencil,
  RefreshCw,
  Settings,
  SquareTerminal,
  Trash2,
  Workflow,
  X,
} from "lucide";

// Constant markup (an eight-ray spark in Claude's orange), so setting it with innerHTML is safe.
export const CLAUDE_ICON = `<svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true">
  <g stroke="#d97757" stroke-width="2.6" stroke-linecap="round">
    <path d="M12 2.5v6M12 15.5v6M2.5 12h6M15.5 12h6" />
    <path d="M5.6 5.6l3.6 3.6M14.8 14.8l3.6 3.6M18.4 5.6l-3.6 3.6M9.2 14.8l-3.6 3.6" />
  </g>
</svg>`;

/** The Lucide icons raterm uses, by name: for `data-icon` attributes in index.html and for `icon()`. */
const ICONS = {
  archive: Archive,
  "archive-restore": ArchiveRestore,
  "arrow-down": ArrowDown,
  "arrow-up": ArrowUp,
  "chevron-down": ChevronDown,
  "chevron-right": ChevronRight,
  file: File,
  folder: Folder,
  "folder-cog": FolderCog,
  "folder-open": FolderOpen,
  house: House,
  menu: Menu,
  pencil: Pencil,
  "refresh-cw": RefreshCw,
  settings: Settings,
  "square-terminal": SquareTerminal,
  "trash-2": Trash2,
  workflow: Workflow,
  x: X,
};

export type IconName = keyof typeof ICONS;

/** The icon as an SVG element, `size` pixels square. */
export function icon(name: IconName, size = 16): SVGElement {
  return createElement(ICONS[name], { width: size, height: size, class: "lucide", "aria-hidden": "true" });
}

/**
 * Fills each `[data-icon]` element under `root` with its icon (`data-size` sets the
 * size). An unknown name is logged and left empty, so a typo cannot stop startup.
 */
export function fillIcons(root: ParentNode = document) {
  for (const el of root.querySelectorAll<HTMLElement>("[data-icon]")) {
    const name = el.dataset.icon ?? "";
    if (!Object.hasOwn(ICONS, name)) {
      console.error("raterm: unknown icon", name);
      continue;
    }
    el.replaceChildren(icon(name as IconName, Number(el.dataset.size) || 16));
  }
}
