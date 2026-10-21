import {
  ChevronDown,
  ChevronRight,
  createElement,
  File,
  Folder,
  FolderCog,
  FolderOpen,
  House,
  Menu,
  RefreshCw,
  Settings,
  X,
} from "lucide";

/** The Lucide icons raterm uses, by name: for `data-icon` attributes in index.html and for `icon()`. */
const ICONS = {
  "chevron-down": ChevronDown,
  "chevron-right": ChevronRight,
  file: File,
  folder: Folder,
  "folder-cog": FolderCog,
  "folder-open": FolderOpen,
  house: House,
  menu: Menu,
  "refresh-cw": RefreshCw,
  settings: Settings,
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
