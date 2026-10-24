import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { icon, type IconName } from "./icons";

/** Mirrors `db::FlowSummary` in src-tauri/src/db.rs. */
export interface Flow {
  id: number;
  name: string;
  archived: boolean;
}

export interface FlowsOptions {
  /** The Flows toolbox in the sidebar. */
  box: HTMLElement;
  /** The Flows settings page, where flows are added, renamed, reordered, archived and deleted. */
  page: HTMLElement;
  errorText(err: unknown): string;
  /** Shows (or clears, with null) an error on the settings page. */
  showError(message: string | null): void;
}

export interface Flows {
  /** Reads the flows again and shows them in the toolbox and on the page. */
  reload(): Promise<void>;
}

/** The Flows toolbox and its settings page; every change is stored right away. */
export function createFlows(opts: FlowsOptions): Flows {
  const boxList = opts.box.querySelector<HTMLUListElement>(".flows")!;
  const boxEmpty = opts.box.querySelector<HTMLElement>(".empty")!;
  const boxError = opts.box.querySelector<HTMLElement>(".error")!;
  const activeList = opts.page.querySelector<HTMLUListElement>(".flow-list")!;
  const archivedList = opts.page.querySelector<HTMLUListElement>(".archived-list")!;
  const pageEmpty = opts.page.querySelector<HTMLElement>(".empty")!;
  const newForm = opts.page.querySelector<HTMLFormElement>("form.new-flow")!;
  const newName = newForm.elements.namedItem("name") as HTMLInputElement;
  let flows: Flow[] = [];
  /** Changes run one after another, each on the flows the previous one left. */
  let queue = Promise.resolve();

  async function reload() {
    try {
      flows = await invoke<Flow[]>("flows_list");
      boxError.hidden = true;
    } catch (err) {
      console.error("raterm: could not read the flows", err);
      boxError.textContent = `Flows could not be read: ${opts.errorText(err)}`;
      boxError.hidden = false;
      throw err;
    } finally {
      render();
    }
  }

  /**
   * Queues a change, then shows the flows as stored, with the change's error if it
   * failed and a note if the flows could not be read back. Resolves to whether the
   * change was made.
   */
  function change(what: string, task: () => Promise<unknown>): Promise<boolean> {
    const run = queue.then(async () => {
      let error = "";
      try {
        await task();
      } catch (err) {
        console.error(`raterm: could not ${what}`, err);
        error = `Could not ${what}: ${opts.errorText(err)}. `;
      }
      try {
        await reload();
      } catch (err) {
        error += `Flows could not be read (${opts.errorText(err)}), so this list may be out of date.`;
      }
      opts.showError(error || null);
      return !error.startsWith("Could not");
    });
    queue = run.then(() => {});
    return run;
  }

  function button(cls: string, name: IconName, title: string, onclick: () => void): HTMLButtonElement {
    const b = document.createElement("button");
    b.type = "button";
    b.className = cls;
    b.title = title;
    b.append(icon(name, 14));
    b.onclick = onclick;
    return b;
  }

  /**
   * Swaps the flow with the next unarchived flow before (-1) or after (1) it, in the
   * flows as they are when the change runs; archived flows keep their places.
   */
  function move(id: number, by: -1 | 1) {
    change("reorder the flows", async () => {
      const active = flows.filter((f) => !f.archived).map((f) => f.id);
      const other = active[active.indexOf(id) + by];
      if (!active.includes(id) || other === undefined) return;
      const ids = flows.map((f) => f.id);
      const [i, j] = [ids.indexOf(id), ids.indexOf(other)];
      [ids[i], ids[j]] = [ids[j], ids[i]];
      await invoke("flows_reorder", { ids });
    });
  }

  async function remove(flow: Flow) {
    const sure = await ask(`Delete the flow "${flow.name}"? This cannot be undone.`, { title: "Delete flow", kind: "warning" });
    if (sure) change(`delete "${flow.name}"`, () => invoke("flow_delete", { id: flow.id }));
  }

  function nameSpan(flow: Flow): HTMLSpanElement {
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = flow.name;
    return name;
  }

  function activeRow(flow: Flow, first: boolean, last: boolean): HTMLLIElement {
    const li = document.createElement("li");
    const name = document.createElement("input");
    name.className = "name";
    name.value = flow.name;
    name.setAttribute("aria-label", "Flow name");
    name.onchange = () => change(`rename "${flow.name}"`, () => invoke("flow_rename", { id: flow.id, name: name.value }));
    const up = button("up", "arrow-up", "Move up", () => move(flow.id, -1));
    const down = button("down", "arrow-down", "Move down", () => move(flow.id, 1));
    up.disabled = first;
    down.disabled = last;
    const archive = button("archive", "archive", "Archive (hide from the sidebar)", () =>
      change(`archive "${flow.name}"`, () => invoke("flow_set_archived", { id: flow.id, archived: true })),
    );
    li.append(name, up, down, archive, button("delete", "trash-2", "Delete", () => remove(flow)));
    return li;
  }

  function archivedRow(flow: Flow): HTMLLIElement {
    const li = document.createElement("li");
    const unarchive = button("unarchive", "archive-restore", "Unarchive", () =>
      change(`unarchive "${flow.name}"`, () => invoke("flow_set_archived", { id: flow.id, archived: false })),
    );
    li.append(nameSpan(flow), unarchive, button("delete", "trash-2", "Delete", () => remove(flow)));
    return li;
  }

  function render() {
    const active = flows.filter((f) => !f.archived);
    const archived = flows.filter((f) => f.archived);
    boxList.replaceChildren(
      ...active.map((flow) => {
        // Not clickable yet: the flow editor comes with the next phase.
        const li = document.createElement("li");
        li.title = flow.name;
        li.append(icon("workflow", 14), nameSpan(flow));
        return li;
      }),
    );
    boxEmpty.hidden = active.length > 0 || !boxError.hidden;
    activeList.replaceChildren(...active.map((f, i) => activeRow(f, i === 0, i === active.length - 1)));
    archivedList.replaceChildren(...archived.map(archivedRow));
    pageEmpty.hidden = active.length > 0;
    archivedList.closest("details")!.hidden = archived.length === 0;
  }

  newForm.onsubmit = async (e) => {
    e.preventDefault();
    const created = await change("add the flow", () => invoke("flow_create", { name: newName.value }));
    if (created) newName.value = "";
  };

  return { reload };
}
