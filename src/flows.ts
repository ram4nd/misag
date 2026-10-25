import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { icon, type IconName } from "./icons";

/** Mirrors the active statuses of `runs::Status` in src-tauri/src/runs.rs. */
export type RunStatus = "running" | "manual" | "attention";

/** Mirrors `runs::Run`. */
export interface Run {
  id: number;
  flowId: number;
  flowName: string;
  tabId: string;
  stepId: string;
  step: string;
  status: RunStatus;
  note: string | null;
  outputs: string[];
  steps: number;
}

const STATUS_LABELS: Record<RunStatus, string> = { running: "Running", manual: "Waiting for you", attention: "Needs attention" };

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
  /** Opens a flow's editor. */
  onOpen(flow: Flow): void;
  /** Called with the flows each time they have been read. */
  onChange(flows: Flow[]): void;
  /** Starts the flow (the ▶ button). */
  onRun(flow: Flow): void;
  /** The person picked `output` for a waiting run, with a note for Claude. */
  onContinue(run: Run, output: string, note: string): Promise<void>;
  /** Stops the run (its Stop button). */
  onStop(run: Run): void;
  /** Shows the run's tab. */
  onShowRun(run: Run): void;
  /** The label of the tab with this id; null when no such tab is open. */
  tabName(tabId: string): string | null;
}

export interface Flows {
  /** Reads the flows again and shows them in the toolbox and on the page. */
  reload(): Promise<void>;
  /** Shows the active runs under their flows in the toolbox. */
  showRuns(runs: Run[]): void;
  /** Shows (or clears, with null) an error in the toolbox. */
  showBoxError(message: string | null): void;
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
  let runs: Run[] = [];
  /** Changes run one after another, each on the flows the previous one left. */
  let queue = Promise.resolve();

  async function reload() {
    try {
      flows = await invoke<Flow[]>("flows_list");
      boxError.hidden = true;
      opts.onChange(flows);
    } catch (err) {
      console.error("misag: could not read the flows", err);
      showBoxError(`Flows could not be read: ${opts.errorText(err)}`);
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
        console.error(`misag: could not ${what}`, err);
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
    const edit = button("edit", "pencil", "Edit the steps", () => opts.onOpen(flow));
    li.append(name, edit, up, down, archive, button("delete", "trash-2", "Delete", () => remove(flow)));
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

  function showBoxError(message: string | null) {
    boxError.hidden = !message;
    boxError.textContent = message ?? "";
    boxEmpty.hidden = flows.some((f) => !f.archived) || !boxError.hidden;
  }

  /**
   * A run under its flow in the toolbox: where it is and, when its tab is open, its
   * step's outputs to go on by (for a running step too, in case no reply comes).
   */
  function runRow(run: Run): HTMLLIElement {
    const li = document.createElement("li");
    li.className = `run ${run.status}`;
    const tab = opts.tabName(run.tabId);
    const summary = document.createElement("div");
    summary.className = "summary";
    summary.textContent = `${tab ?? "closed tab"} · ${run.step} · ${STATUS_LABELS[run.status]}`;
    summary.title = `${run.flowName}: ${run.steps} step${run.steps === 1 ? "" : "s"} since you last stepped in`;
    if (tab !== null) summary.onclick = () => opts.onShowRun(run);
    li.append(summary);
    if (run.note) li.append(Object.assign(document.createElement("p"), { className: "note", textContent: run.note }));
    const actions = document.createElement("div");
    actions.className = "actions";
    if (tab !== null && run.outputs.length > 0) {
      const note = document.createElement("input");
      note.className = "run-note";
      note.dataset.run = String(run.id);
      note.placeholder = "Note for Claude (optional)";
      note.setAttribute("aria-label", "Note for Claude");
      const buttons = run.outputs.map((output) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "output";
        b.textContent = output;
        b.title = run.status === "running" ? `Claude is still on this step; go on as "${output}" anyway` : `Go on as "${output}"`;
        b.onclick = async () => {
          // Once: a second click would send the next step too.
          for (const other of buttons) other.disabled = true;
          await opts.onContinue(run, output, note.value);
          for (const other of buttons) other.disabled = false;
        };
        return b;
      });
      actions.append(...buttons);
      li.append(note);
    }
    actions.append(button("stop-run", "square", "Stop this run", () => opts.onStop(run)));
    li.append(actions);
    return li;
  }

  function render() {
    const active = flows.filter((f) => !f.archived);
    const archived = flows.filter((f) => f.archived);
    boxList.replaceChildren(
      ...active.map((flow) => {
        const li = document.createElement("li");
        li.title = `Edit ${flow.name}`;
        li.onclick = () => opts.onOpen(flow);
        const run = button("run", "play", "Run in the active Claude tab", () => opts.onRun(flow));
        run.addEventListener("click", (e) => e.stopPropagation());
        li.append(icon("workflow", 14), nameSpan(flow), run);
        const mine = runs.filter((r) => r.flowId === flow.id);
        if (mine.length > 0) {
          const list = document.createElement("ul");
          list.className = "runs";
          // Clicks in a run are about the run, not about opening the flow.
          list.onclick = (e) => e.stopPropagation();
          list.append(...mine.map(runRow));
          li.append(list);
        }
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

  return {
    reload,
    showBoxError,
    showRuns(next: Run[]) {
      if (JSON.stringify(next) === JSON.stringify(runs)) return;
      // Notes being typed, and the focus, are carried over to the new rows.
      const notes = new Map([...boxList.querySelectorAll<HTMLInputElement>("input.run-note")].map((i) => [i.dataset.run, i]));
      const focused = document.activeElement instanceof HTMLInputElement ? document.activeElement.dataset.run : undefined;
      runs = next;
      render();
      for (const input of boxList.querySelectorAll<HTMLInputElement>("input.run-note")) {
        const old = notes.get(input.dataset.run);
        if (!old) continue;
        input.value = old.value;
        if (input.dataset.run === focused) {
          input.focus();
          input.setSelectionRange(old.selectionStart, old.selectionEnd);
        }
      }
    },
  };
}
