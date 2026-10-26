import { invoke } from "@tauri-apps/api/core";
import { createCanvas } from "./canvas";
import { STATUS_LABELS, type Run } from "./flows";

/** Mirrors `flows::Kind` in src-tauri/src/flows.rs. */
export type StepKind = "start" | "prompt" | "manual" | "end";

/** Mirrors `flows::Node`. */
export interface Step {
  id: string;
  kind: StepKind;
  title: string;
  /** A prompt, what the person does, or for Start: optional base info sent ahead of the first prompt. */
  text: string;
  outputs: string[];
  /** How many times a run may take the step before it waits for the person; absent: no limit. */
  maxRuns?: number;
  /** For Start: a run begins by typing /clear, in a new conversation; absent: it does not. */
  clear?: boolean;
  x: number;
  y: number;
}

/** Mirrors `flows::Graph`. */
export interface Graph {
  version: number;
  nodes: Step[];
  edges: { from: string; output: string; to: string }[];
}

/** Mirrors `commands::FlowDetail`. */
interface FlowDetail {
  id: number;
  name: string;
  archived: boolean;
  graph: Graph;
  problems: string[];
}

/** How long the editor waits after a change before saving, so a burst of edits is one save. */
const SAVE_DELAY_MS = 400;
/** Outputs as the backend accepts them (see `flows::plain_output`); "attention" is reserved. */
const OUTPUT = /^[a-z0-9][a-z0-9_-]{0,23}$/;
/** The backend's limits (`flows::MAX_STEPS`, `MAX_OUTPUTS`, `MAX_RUNS`, `MAX_TEXT`, `MAX_INFO`, and the title length). */
const MAX_STEPS = 200;
const MAX_TEXT = 20000;
const MAX_INFO = 4000;
const MAX_OUTPUTS = 8;
const MAX_RUNS = 100;
/** The steps the toolbar adds. There is no End to add: an output with no next step ends the flow. */
const NEW_STEPS: Record<"prompt" | "manual", { title: string; outputs: string[] }> = {
  prompt: { title: "Prompt", outputs: ["done"] },
  manual: { title: "Manual step", outputs: ["done"] },
};

export interface FlowEditorOptions {
  errorText(err: unknown): string;
  /** The person picked `output` for the shown run's step; resolves to why it could not go on, or null. */
  onContinue(run: Run, output: string): Promise<string | null>;
  /** Has Claude go on with the run's step that needs attention; resolves like `onContinue`. */
  onResume(run: Run): Promise<string | null>;
}

export interface FlowEditor {
  /** The editor's page, to be put where pages are shown. */
  el: HTMLElement;
  /** Reads the flow and draws it; false (with the reason shown) if it cannot be read. */
  load(): Promise<boolean>;
  /** Call when the page is shown, so the canvas can measure itself. */
  shown(): void;
  /** Saves any change still waiting; false if the latest changes are not saved. */
  flush(): Promise<boolean>;
  /** Drops any change still waiting, for a flow that is gone. */
  discard(): void;
  /** Shows where the active tab's run of this flow is (nothing with null), and its outputs to go on by. */
  showRun(run: Run | null): void;
}

function editorMarkup(): string {
  return `
    <form novalidate>
      <fieldset>
        <div class="toolbar">
          <button type="button" class="add-prompt">+ Prompt</button>
          <button type="button" class="add-manual">+ Manual step</button>
          <span class="status" role="status"></span>
        </div>
        <div class="run-bar" hidden>
          <span class="where"></span>
          <span class="actions"></span>
          <p class="note"></p>
          <p class="run-error" role="alert" hidden></p>
        </div>
        <div class="workspace">
          <div class="canvas"></div>
          <aside class="side">
            <div class="panel" hidden>
              <label>Title <input name="title" maxlength="200" /></label>
              <label><span class="text-label">Prompt</span> <textarea name="text" rows="8" maxlength="20000"></textarea></label>
              <label>Outputs <input name="outputs" />
                <span class="hint">Comma-separated short words, such as <code>yes, no</code>. Each can lead to a next step (an earlier one makes a loop); an output that leads nowhere ends the flow.</span>
              </label>
              <label>Run at most (times) <input name="maxRuns" type="number" min="1" placeholder="no limit" />
                <span class="hint">For a step a loop comes back to, such as a review after a fix: once it has run this many times, the run waits for you before taking it again. Leave empty for no limit.</span>
              </label>
              <label class="check"><input name="clear" type="checkbox" /> Start with /clear
                <span class="hint">Each run first clears the conversation, so Claude begins the flow without what came before.</span>
              </label>
              <p class="invalid" role="alert" hidden></p>
              <p class="invalid runs-invalid" role="alert" hidden></p>
              <button type="button" class="delete-step">Delete step</button>
            </div>
            <div class="problems"></div>
          </aside>
        </div>
      </fieldset>
      <p class="error" role="alert" hidden></p>
    </form>`;
}

/** The editor tab of flow `flowId`: a canvas of its steps, and a panel for the selected one. */
export function createFlowEditor(flowId: number, opts: FlowEditorOptions): FlowEditor {
  const el = document.createElement("section");
  el.className = "page flow-editor";
  el.dataset.flow = String(flowId);
  el.tabIndex = -1;
  el.innerHTML = editorMarkup();
  const $ = <T extends Element>(sel: string) => el.querySelector<T>(sel)!;
  const form = $<HTMLFormElement>("form");
  const panel = $<HTMLElement>(".panel");
  const title = $<HTMLInputElement>("[name=title]");
  const text = $<HTMLTextAreaElement>("[name=text]");
  const outputs = $<HTMLInputElement>("[name=outputs]");
  const maxRuns = $<HTMLInputElement>("[name=maxRuns]");
  const clear = $<HTMLInputElement>("[name=clear]");
  maxRuns.max = String(MAX_RUNS);
  const invalidRuns = $<HTMLElement>(".runs-invalid");
  const invalid = $<HTMLElement>(".invalid:not(.runs-invalid)");
  const status = $<HTMLElement>(".status");
  const errorEl = $<HTMLElement>(".error");
  form.onsubmit = (e) => e.preventDefault();

  let graph: Graph = { version: 1, nodes: [], edges: [] };
  let selected: string | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  /** Saves run one after another, so an older graph never lands last. */
  let saving = Promise.resolve();
  /** Whether the last save worked, so a tab is not closed on unsaved changes. */
  let saved = true;

  const canvas = createCanvas($(".canvas"), {
    moved(id, x, y) {
      const s = find(id);
      // A position that is not a number cannot be stored as JSON; such a move is dropped.
      if (!s || !Number.isFinite(x) || !Number.isFinite(y)) return;
      [s.x, s.y] = [x, y];
      changed(false);
    },
    connected(from, output, to) {
      // An output leads to one step: a new connection replaces the old one.
      graph.edges = graph.edges.filter((e) => !(e.from === from && e.output === output));
      graph.edges.push({ from, output, to });
      changed(true);
    },
    disconnected(from, output, to) {
      graph.edges = graph.edges.filter((e) => !(e.from === from && e.output === output && e.to === to));
      changed(false);
    },
    removed(id) {
      removeStep(id);
    },
    selected(id) {
      // Edits not yet committed (they commit on change) are kept.
      if (find(selected) && !panel.hidden) {
        commitOutputs();
        commitMaxRuns();
      }
      selected = id;
      showPanel();
    },
  });

  const find = (id: string | null) => graph.nodes.find((s) => s.id === id);

  function showError(message: string | null) {
    errorEl.hidden = !message;
    errorEl.textContent = message ?? "";
  }

  function showProblems(problems: string[]) {
    const box = $<HTMLElement>(".problems");
    if (problems.length === 0) {
      box.textContent = "Ready to run.";
      return;
    }
    const heading = document.createElement("p");
    heading.textContent = "Before this flow can run:";
    const list = document.createElement("ul");
    list.append(...problems.map((p) => Object.assign(document.createElement("li"), { textContent: p })));
    box.replaceChildren(heading, list);
  }

  function showPanel() {
    const s = find(selected);
    panel.hidden = !s;
    invalid.hidden = true;
    invalidRuns.hidden = true;
    if (!s) return;
    title.value = s.title;
    text.value = s.text;
    outputs.value = s.outputs.join(", ");
    maxRuns.value = s.maxRuns === undefined ? "" : String(s.maxRuns);
    const words = s.kind === "prompt" || s.kind === "manual";
    text.closest("label")!.hidden = !words && s.kind !== "start";
    // A manual step goes on one way, with ▶; only one from an older flow, with several, shows them to cut down.
    outputs.closest("label")!.hidden = !(s.kind === "prompt" || (s.kind === "manual" && s.outputs.length > 1));
    // Leaving a manual step is always your pick, which no limit holds back.
    maxRuns.closest("label")!.hidden = s.kind !== "prompt";
    clear.checked = s.clear === true;
    clear.closest("label")!.hidden = s.kind !== "start";
    $(".text-label").textContent = { start: "Base info", prompt: "Prompt", manual: "What the person does", end: "" }[s.kind];
    text.maxLength = s.kind === "start" ? MAX_INFO : MAX_TEXT;
    text.placeholder = s.kind === "start" ? "Optional: where things are and what Claude should know, sent ahead of the first prompt" : "";
    $<HTMLButtonElement>(".delete-step").hidden = s.kind === "start";
  }

  /** Records a change to the graph: redraws the canvas if what it shows changed, and saves soon. */
  function changed(redraw: boolean) {
    if (redraw) canvas.render(graph);
    showWhere();
    status.textContent = "Saving…";
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => void save(), SAVE_DELAY_MS);
  }

  function save(): Promise<void> {
    timer = null;
    const snapshot = structuredClone(graph);
    saving = saving.then(async () => {
      try {
        showProblems(await invoke<string[]>("flow_save_graph", { id: flowId, graph: snapshot }));
        showError(null);
        saved = true;
      } catch (err) {
        console.error("misag: could not save the flow", err);
        showError(`Not saved: ${opts.errorText(err)}. Your latest changes are not stored; the next change tries again.`);
        $<HTMLElement>(".problems").textContent = "Not checked: the latest changes are not saved.";
        saved = false;
      }
      if (timer === null) status.textContent = saved ? "Saved" : "";
    });
    return saving;
  }

  function removeStep(id: string) {
    if (find(id)?.kind === "start") {
      // Drawflow removed it anyway; draw it back.
      canvas.render(graph);
      return;
    }
    graph.nodes = graph.nodes.filter((s) => s.id !== id);
    graph.edges = graph.edges.filter((e) => e.from !== id && e.to !== id);
    if (selected === id) selected = null;
    showPanel();
    changed(true);
  }

  function addStep(kind: keyof typeof NEW_STEPS) {
    const n = graph.nodes.length;
    if (n >= MAX_STEPS) {
      showError(`A flow has at most ${MAX_STEPS} steps.`);
      return;
    }
    const { title, outputs } = NEW_STEPS[kind];
    graph.nodes.push({
      // Never an id a deleted step had: a run's history refers to steps by id.
      id: `s${[...crypto.getRandomValues(new Uint8Array(4))].map((b) => b.toString(16).padStart(2, "0")).join("")}`,
      kind,
      title,
      text: "",
      outputs: [...outputs],
      x: 40 + 260 * (n % 4),
      y: 40 + 180 * Math.floor(n / 4),
    });
    changed(true);
  }

  $<HTMLButtonElement>(".add-prompt").onclick = () => addStep("prompt");
  $<HTMLButtonElement>(".add-manual").onclick = () => addStep("manual");
  $<HTMLButtonElement>(".delete-step").onclick = () => {
    const s = find(selected);
    if (s && s.kind !== "start") removeStep(s.id);
  };
  title.oninput = () => {
    const s = find(selected);
    if (!s) return;
    s.title = title.value;
    canvas.retitle(s.id, s.title);
    changed(false);
  };
  text.oninput = () => {
    const s = find(selected);
    if (!s) return;
    // The canvas marks a Start with base info.
    const marked = s.kind === "start" && s.text.trim() !== "";
    s.text = text.value;
    changed(s.kind === "start" && marked !== (s.text.trim() !== ""));
  };
  /** Applies the outputs field to the selected step, or says why it cannot. */
  function commitOutputs() {
    const s = find(selected);
    if (!s || (s.kind !== "prompt" && s.kind !== "manual") || outputs.value === s.outputs.join(", ")) return;
    const words = [...new Set(outputs.value.split(",").map((w) => w.trim().toLowerCase()).filter((w) => w !== ""))];
    const bad = words.filter((w) => !OUTPUT.test(w) || w === "attention");
    const why =
      bad.length > 0
        ? `Not an output: ${bad.map((w) => `"${w}"`).join(", ")}. Use short words of letters, digits, - and _ ("attention" is reserved).`
        : words.length === 0
          ? "A step needs at least one output."
          : words.length > MAX_OUTPUTS
            ? `A step has at most ${MAX_OUTPUTS} outputs.`
            : s.kind === "manual" && words.length > 1
              ? "A manual step has one output: ▶ goes on from it, and ■ stops the run."
              : null;
    invalid.hidden = why === null;
    invalid.textContent = why ?? "";
    if (why !== null) return;
    s.outputs = words;
    // Connections from outputs that are gone go with them.
    graph.edges = graph.edges.filter((e) => e.from !== s.id || s.outputs.includes(e.output));
    outputs.value = s.outputs.join(", ");
    changed(true);
  }
  outputs.onchange = commitOutputs;
  clear.onchange = () => {
    const s = find(selected);
    if (!s || s.kind !== "start") return;
    if (clear.checked) s.clear = true;
    else delete s.clear;
    changed(true);
  };
  /** Applies the run limit field to the selected prompt step, or says why it cannot. */
  function commitMaxRuns() {
    const s = find(selected);
    if (!s || s.kind !== "prompt") return;
    const value = maxRuns.value.trim();
    const n = Number(value);
    // A number field reads text it cannot parse ("e", "-") as empty: that is not "no limit".
    const ok = !maxRuns.validity.badInput && (value === "" || (Number.isInteger(n) && n >= 1 && n <= MAX_RUNS));
    invalidRuns.hidden = ok;
    invalidRuns.textContent = ok ? "" : `A step runs 1 to ${MAX_RUNS} times; leave it empty for no limit.`;
    const next = value === "" ? undefined : n;
    if (!ok || next === s.maxRuns) return;
    if (next === undefined) delete s.maxRuns;
    else s.maxRuns = next;
    // The canvas shows the limit.
    changed(true);
  }
  maxRuns.onchange = commitMaxRuns;

  async function load(): Promise<boolean> {
    try {
      const flow = await invoke<FlowDetail>("flow_load", { id: flowId });
      graph = flow.graph;
      canvas.render(graph);
      showWhere();
      showProblems(flow.problems);
      showError(null);
      return true;
    } catch (err) {
      console.error("misag: could not load the flow", err);
      showError(`The flow could not be loaded (${opts.errorText(err)}). Close and reopen this tab to try again.`);
      return false;
    }
  }

  async function flush(): Promise<boolean> {
    commitOutputs();
    commitMaxRuns();
    // A save that failed before is tried again too.
    if (timer !== null || !saved) {
      if (timer !== null) clearTimeout(timer);
      await save();
    }
    await saving;
    return saved;
  }

  const runBar = $<HTMLElement>(".run-bar");
  const runError = $<HTMLElement>(".run-bar .run-error");
  let run: Run | null = null;
  /** JSON.stringify(null): the bar starts hidden, as for no run. */
  let shownRun = "null";

  function showRunError(message: string | null) {
    runError.hidden = !message;
    runError.textContent = message ?? "";
  }

  /** Where the run is; its step may be gone from the flow, as the run follows the flow it started on. */
  function showWhere() {
    if (!run) return;
    const gone = graph.nodes.some((s) => s.id === run!.stepId) ? "" : " (no longer in this flow)";
    $(".run-bar .where").textContent = `${STATUS_LABELS[run.status]}: ${run.step}${gone}`;
  }

  /**
   * Shows where the run is, and offers its step's outputs (as the flow was when the
   * run started), for a running step too, in case Claude was interrupted and no reply comes.
   */
  function showRun(next: Run | null) {
    // Polled often: redrawn only on a change, so a click is not lost to a redraw.
    if (JSON.stringify(next) === shownRun) return;
    shownRun = JSON.stringify(next);
    run = next;
    canvas.mark(run?.stepId ?? null);
    showRunError(null);
    runBar.hidden = !run;
    runBar.classList.remove("running", "manual", "attention");
    if (!run) return;
    runBar.classList.add(run.status);
    showWhere();
    const note = $<HTMLElement>(".run-bar .note");
    note.hidden = !run.note;
    note.textContent = run.note ?? "";
    const shown = run;
    /** Runs a button's action once: a second click would send the next step too. */
    async function once(go: () => Promise<string | null>) {
      for (const other of buttons) other.disabled = true;
      showRunError(null);
      try {
        showRunError(await go());
      } finally {
        for (const other of buttons) other.disabled = false;
      }
    }
    const buttons = shown.outputs.map((output) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "output";
      b.textContent = output;
      b.title = shown.status === "running" ? `Claude is still on this step; go on as "${output}" anyway` : `Go on as "${output}"`;
      b.onclick = () => once(() => opts.onContinue(shown, output));
      return b;
    });
    if (shown.resumable) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "resume";
      b.textContent = "Continue";
      b.title = "Have Claude go on with this step";
      b.onclick = () => once(() => opts.onResume(shown));
      buttons.unshift(b);
    }
    $(".run-bar .actions").replaceChildren(...buttons);
  }

  function discard() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  return { el, load, shown: () => canvas.refresh(), flush, discard, showRun };
}
