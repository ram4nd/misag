import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import type { StepKind } from "./flowEditor";
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
  /** How many of the flow's steps (all but Start and End) are done, each once however often it ran, and how many there are. */
  done: number;
  total: number;
  /** Whether Claude can be asked to go on with the step (a prompt step that needs attention). */
  resumable: boolean;
  /** Whether the app has yet to fill Claude's input for the manual prompt the run waits at. */
  prefill: boolean;
  /** The kind of the step the run is at; null for a run that cannot be read. */
  stepKind: StepKind | null;
  /** Whether the run waits at Start for Claude's reply to its briefing. */
  briefing: boolean;
}

export const STATUS_LABELS: Record<RunStatus, string> = { running: "Running", manual: "Waiting for you", attention: "Needs attention" };

/** Whether the run waits for the person's own prompt, at a manual prompt (not while Claude answers it). */
export const awaitsPrompt = (run: Run) => run.stepKind === "manual_prompt" && run.status !== "running";

/** What the run is doing, for the person. */
export function statusLabel(run: Run): string {
  if (awaitsPrompt(run) && run.status === "manual") return "Waiting for your prompt";
  if (run.briefing) return "Briefing Claude";
  return STATUS_LABELS[run.status];
}

/** Mirrors `db::FlowSummary` in src-tauri/src/db.rs. */
export interface Flow {
  id: number;
  name: string;
  archived: boolean;
  /** Whether a run begins by typing /clear; absent: it does not. */
  clear?: boolean;
  /** Added by a program in a tab and not yet confirmed, so ▶ asks first; absent: it was not. */
  unconfirmed?: boolean;
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
  /** Stops the run (the ■ button that takes ▶'s place). */
  onStop(run: Run): void;
  /**
   * Goes on from a run waiting for you (a Pause, a Manual prompt, a step that needs
   * attention) as `output`: ▶ for its only output, else its buttons under the flow.
   */
  onContinue(run: Run, output: string): void;
}

export interface Flows {
  /** Reads the flows again and shows them in the toolbox and on the page. */
  reload(): Promise<void>;
  /** Shows flows changed outside the app (`misag flow import`); only redraws on a change. */
  refresh(): Promise<void>;
  /**
   * Marks the flows of the active tab's runs, waiting or not, and of runs whose tab
   * is gone (which can only be stopped); ▶ is off on the rest while the tab runs one.
   */
  showRuns(here: Run[], orphans: Run[]): void;
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
  let here: Run[] = [];
  let orphans: Run[] = [];
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

  /**
   * Adds a flow from a file the person picks, and opens it, so its problems show.
   * The backend opens the file dialogs, so the page never names a file.
   */
  async function importFile() {
    let imported: Flow | null = null;
    const done = await change("import the flow", async () => {
      imported = await invoke<Flow | null>("flow_import_file");
    });
    const flow = imported as Flow | null;
    if (done && flow) opts.onOpen(flow);
  }

  /** Writes the flow to a file the person picks. */
  async function exportFile(flow: Flow) {
    try {
      await invoke<boolean>("flow_export_file", { id: flow.id });
      opts.showError(null);
    } catch (err) {
      console.error("misag: could not export the flow", err);
      opts.showError(`Could not export "${flow.name}": ${opts.errorText(err)}`);
    }
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
    const exported = button("export", "download", "Export to a file", () => exportFile(flow));
    li.append(name, edit, exported, up, down, archive, button("delete", "trash-2", "Delete", () => remove(flow)));
    return li;
  }

  function archivedRow(flow: Flow): HTMLLIElement {
    const li = document.createElement("li");
    const unarchive = button("unarchive", "archive-restore", "Unarchive", () =>
      change(`unarchive "${flow.name}"`, () => invoke("flow_set_archived", { id: flow.id, archived: false })),
    );
    const exported = button("export", "download", "Export to a file", () => exportFile(flow));
    li.append(nameSpan(flow), exported, unarchive, button("delete", "trash-2", "Delete", () => remove(flow)));
    return li;
  }

  function showBoxError(message: string | null) {
    boxError.hidden = !message;
    boxError.textContent = message ?? "";
    boxEmpty.hidden = flows.some((f) => !f.archived) || !boxError.hidden;
  }

  function render() {
    const active = flows.filter((f) => !f.archived);
    const archived = flows.filter((f) => f.archived);
    boxList.replaceChildren(
      ...active.map((flow) => {
        const li = document.createElement("li");
        const mine = here.find((r) => r.flowId === flow.id);
        const orphan = mine ? undefined : orphans.find((r) => r.flowId === flow.id);
        const run = mine ?? orphan;
        li.title = mine
          ? `${flow.name}: ${statusLabel(mine).toLowerCase()} at ${mine.step}, ${mine.done} of ${mine.total} steps done${mine.note ? `. ${mine.note}` : ""}`
          : orphan
            ? `${flow.name}: running in a closed tab; ■ stops it`
            : `Edit ${flow.name}`;
        li.onclick = () => opts.onOpen(flow);
        const action = run
          ? button("stop-run", "square", "Stop this run", () => opts.onStop(run))
          : button("run", "play", "Run in the active Claude tab", () => opts.onRun(flow));
        action.addEventListener("click", (e) => e.stopPropagation());
        if (!run && here.length > 0) {
          // The backend would refuse it.
          action.disabled = true;
          action.title = `This tab runs ${here[0].flowName}; a tab runs one flow at a time`;
        }
        if (run) li.classList.add("running", orphan ? "orphan" : run.status);
        const attention = mine?.status === "attention";
        const mark = icon(attention ? "circle-alert" : flow.clear ? "eraser" : "workflow", 14);
        if (attention) mark.classList.add("needs-attention");
        else if (flow.clear) mark.classList.add("clears");
        li.append(mark, nameSpan(flow));
        if (run) {
          const progress = document.createElement("span");
          progress.className = "progress";
          progress.textContent = `${run.done}/${run.total}`;
          li.append(progress);
        }
        li.append(action);
        // A run waiting for you goes on to the next step from here; having Claude go
        // on with a step that needs attention is the editor's Continue.
        const waits = mine !== undefined && mine.status !== "running";
        const title = (run: Run) =>
          awaitsPrompt(run)
            ? `Skip "${run.step}" without sending a prompt`
            : run.status === "attention"
              ? `Go on from "${run.step}" to the next step`
              : `Done with "${run.step}": go on`;
        if (waits && mine.outputs.length === 1) {
          // One way on (a Pause, a Manual prompt to skip, a one-output step): ▶ takes it.
          const go = button("go-on", "play", title(mine), () => {
            go.disabled = true; // A second click would send the next step too; the poll after it redraws the row.
            opts.onContinue(mine, mine.outputs[0]);
          });
          go.addEventListener("click", (e) => e.stopPropagation());
          li.insertBefore(go, action);
        } else if (waits && mine.outputs.length > 1) {
          // Several ways on (a step that needs attention, or an older flow's Pause): its outputs, on a line of their own.
          const picks = document.createElement("div");
          picks.className = "picks";
          picks.append(
            ...mine.outputs.map((output) => {
              const b = document.createElement("button");
              b.type = "button";
              b.className = "pick";
              b.textContent = output;
              b.title = `Go on as "${output}"`;
              b.onclick = (e) => {
                e.stopPropagation();
                // A second click would send the next step too; the poll after the pick redraws the row.
                for (const other of picks.querySelectorAll("button")) other.disabled = true;
                opts.onContinue(mine, output);
              };
              return b;
            }),
          );
          li.append(picks);
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

  opts.page.querySelector<HTMLButtonElement>("button.import")!.onclick = () => void importFile();

  return {
    reload,
    refresh() {
      // Queued, so it never lands between a change and the reload after it.
      const run = queue.then(async () => {
        let next: Flow[];
        try {
          next = await invoke<Flow[]>("flows_list");
        } catch (err) {
          // The next reload (after a change, or when the page opens) shows the error.
          console.error("misag: could not read the flows", err);
          return;
        }
        if (JSON.stringify(next) === JSON.stringify(flows)) return;
        flows = next;
        opts.onChange(flows);
        render();
      });
      // A redraw that throws must not stop every change queued after it.
      queue = run.catch((err) => console.error("misag: could not show the flows", err));
      return queue;
    },
    showBoxError,
    showRuns(nextHere: Run[], nextOrphans: Run[]) {
      if (JSON.stringify([nextHere, nextOrphans]) === JSON.stringify([here, orphans])) return;
      [here, orphans] = [nextHere, nextOrphans];
      render();
    },
  };
}
