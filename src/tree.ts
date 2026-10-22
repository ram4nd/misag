import { CLAUDE_ICON, icon } from "./icons";

/** Mirrors `files::Entry` in src-tauri/src/files.rs. */
export interface Entry {
  name: string;
  dir: boolean;
}

/** Mirrors `files::Listing` in src-tauri/src/files.rs. */
export interface Listing {
  /** The directory listed, with `~` expanded. */
  dir: string;
  entries: Entry[];
  truncated: boolean;
  /** Entries left out: hidden, not UTF-8, or unreadable. */
  skipped: number;
}

export interface TreeOptions {
  /** Lists one directory level; a rejection is shown in the tree. */
  list(dir: string): Promise<Listing>;
  /** The text shown for a rejected listing. */
  errorText(err: unknown): string;
  onPickFile(path: string): void;
  /** Opens Claude in a folder, from the Claude button on the folder's row. */
  onOpenClaude(dir: string): void;
}

export interface Tree {
  /** Roots the tree at `dir` unless it already is; a new root starts with its folders closed. */
  setRoot(dir: string): void;
  /** Lists the root again, keeping open folders open. */
  reload(): void;
  /** The root as listed, with `~` expanded; undefined until it has been listed. */
  listedRoot(): string | undefined;
}

/** A lazily listed directory tree in `el`: each folder is listed when it is opened. */
export function createTree(el: HTMLUListElement, opts: TreeOptions): Tree {
  let root: string | undefined;
  let listedRoot: string | undefined;
  /** Folders the user opened, by path; a reload opens them again. */
  const expanded = new Set<string>();

  function note(text: string, depth: number, error = false): HTMLLIElement {
    const li = document.createElement("li");
    li.className = error ? "note error" : "note";
    li.style.paddingLeft = `${8 + depth * 12}px`;
    li.textContent = text;
    return li;
  }

  /**
   * Fills `ul` with one level of `dir`, opening the folders that were open before. A
   * tree replaced meanwhile only fills its own detached list.
   */
  async function fill(ul: HTMLUListElement, dir: string, depth: number): Promise<Listing | undefined> {
    // A reload keeps what is shown until the new listing arrives.
    if (!ul.children.length) ul.replaceChildren(note("Loading…", depth));
    let listing: Listing;
    try {
      listing = await opts.list(dir);
    } catch (err) {
      console.error("raterm: could not list", dir, err);
      ul.replaceChildren(note(opts.errorText(err), depth, true));
      return undefined;
    }
    const base = listing.dir.replace(/\/+$/, "");
    const items = listing.entries.map((e) => item(`${base}/${e.name}`, e, depth));
    if (!items.length) items.push(note(listing.skipped ? `No visible entries (${listing.skipped} hidden)` : "Empty", depth));
    if (listing.truncated) items.push(note("More entries not shown", depth));
    ul.replaceChildren(...items);
    return listing;
  }

  /** `row` in a line with a button that opens Claude in `dir`, shown on hover or focus. */
  function withClaudeButton(row: HTMLElement, dir: string): HTMLDivElement {
    // Next to the row, not inside it: a button cannot hold another button.
    const claude = document.createElement("button");
    claude.className = "open-claude";
    claude.title = `Open Claude in ${dir}`;
    claude.innerHTML = CLAUDE_ICON;
    claude.onclick = () => opts.onOpenClaude(dir);
    const line = document.createElement("div");
    line.className = "line";
    line.append(row, claude);
    return line;
  }

  function item(path: string, entry: Entry, depth: number): HTMLLIElement {
    const li = document.createElement("li");
    const row = document.createElement("button");
    row.className = "row";
    row.title = path;
    row.style.paddingLeft = `${8 + depth * 12}px`;
    const chevron = document.createElement("span");
    chevron.className = "chevron";
    const kind = document.createElement("span");
    kind.className = "kind";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = entry.name;
    row.append(chevron, kind, name);
    if (!entry.dir) {
      li.append(row);
      kind.append(icon("file", 14));
      row.onclick = () => opts.onPickFile(path);
      return li;
    }
    li.append(withClaudeButton(row, path));
    const children = document.createElement("ul");
    li.append(children);
    const show = (open: boolean) => {
      chevron.replaceChildren(icon(open ? "chevron-down" : "chevron-right", 14));
      kind.replaceChildren(icon(open ? "folder-open" : "folder", 14));
      children.hidden = !open;
      row.setAttribute("aria-expanded", String(open));
      // Listed each time it is opened, so a large tree costs nothing until it is explored.
      if (open) void fill(children, path, depth + 1);
    };
    row.onclick = () => {
      const open = !expanded.has(path);
      if (open) expanded.add(path);
      else expanded.delete(path);
      show(open);
    };
    show(expanded.has(path));
    return li;
  }

  function render(dir: string) {
    const head = document.createElement("li");
    const label = document.createElement("div");
    label.className = "row root";
    label.textContent = dir.replace(/\/+$/, "").split("/").pop() || dir;
    label.title = dir;
    const children = document.createElement("ul");
    head.append(withClaudeButton(label, dir));
    el.replaceChildren(head, children);
    void fill(children, dir, 1).then((listing) => {
      if (listing && root === dir) listedRoot = listing.dir;
    });
  }

  return {
    setRoot(dir) {
      if (dir === root) return;
      root = dir;
      listedRoot = undefined;
      expanded.clear();
      render(dir);
    },
    reload() {
      if (root !== undefined) render(root);
    },
    listedRoot: () => listedRoot,
  };
}
