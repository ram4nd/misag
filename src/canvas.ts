import Drawflow, { type ConnectionEvent } from "drawflow";
import "drawflow/dist/drawflow.min.css";
import type { Graph, Step } from "./flowEditor";

/** What the user did on the canvas, in terms of the flow's steps and outputs. */
export interface CanvasEvents {
  moved(id: string, x: number, y: number): void;
  connected(from: string, output: string, to: string): void;
  disconnected(from: string, output: string, to: string): void;
  /** Deleted with the Delete key; Start cannot be. */
  removed(id: string): void;
  selected(id: string | null): void;
}

export interface Canvas {
  /** Draws the graph, replacing what was drawn before. */
  render(graph: Graph): void;
  /** Shows a step's new title without redrawing the rest. */
  retitle(id: string, title: string): void;
  /** Redraws the connections, which are measured from the steps: needed once the canvas is visible. */
  refresh(): void;
  /** Marks the step a run is at (none with null), through redraws too. */
  mark(id: string | null): void;
}

const KIND_LABELS: Record<Step["kind"], string> = { start: "Start", prompt: "Prompt", manual: "Manual", end: "End" };

/**
 * A Drawflow canvas showing a flow's steps, drawn in an element it adds to `canvasEl`.
 * Drawflow's own node ids stay in here: everything outside uses the flow's step ids.
 */
export function createCanvas(canvasEl: HTMLElement, events: CanvasEvents): Canvas {
  // Drawflow pans only when a drag starts on an element whose first class is its own,
  // so it gets a bare element of its own rather than one styled by misag.
  const el = document.createElement("div");
  canvasEl.append(el);
  const df = new Drawflow(el);
  df.reroute = false;
  let graph: Graph = { version: 1, nodes: [], edges: [] };
  const stepOf = new Map<string, Step>();
  const drawnAs = new Map<string, number>();
  // Drawflow reports what `render` itself draws as if the user did it.
  let rendering = false;
  let marked: string | null = null;

  const step = (dfId: number | string) => stepOf.get(String(dfId));
  /** Resolves a connection event to its from step, output and to step; null if any is unknown. */
  function port(e: ConnectionEvent): [Step, string, Step] | null {
    const from = step(e.output_id);
    const to = step(e.input_id);
    const output = from?.outputs[Number(e.output_class.replace("output_", "")) - 1];
    return from && to && output !== undefined ? [from, output, to] : null;
  }

  // Drawflow deletes the selected node with the Delete key and from its right-click
  // menu. These listeners come before Drawflow's own, so Start is never offered.
  let selected: string | null = null;
  const startSelected = () => selected !== null && step(selected)?.kind === "start";
  el.addEventListener("keydown", (e) => {
    if ((e.key === "Delete" || e.key === "Backspace") && startSelected()) e.stopImmediatePropagation();
  });
  el.addEventListener("contextmenu", (e) => {
    if (startSelected()) e.stopImmediatePropagation();
  });
  /** Logs an event that does not match what was drawn: the canvas and the flow have drifted apart. */
  const dropped = (what: string, detail: unknown) => console.error(`misag: ignored a canvas ${what}`, detail);
  df.start();
  // The wheel scrolls the canvas; Drawflow zooms with Ctrl+wheel. Drawflow 0.0.60 has no
  // call to pan, so this moves it the way its own drag does, through its canvas_x,
  // canvas_y and precanvas properties, and tells its listeners as the drag does.
  if (Number.isFinite(df.canvas_x) && Number.isFinite(df.canvas_y) && Number.isFinite(df.zoom) && df.precanvas && typeof df.dispatch === "function") {
    el.addEventListener("wheel", (e) => {
      if (e.ctrlKey) return;
      e.preventDefault();
      // Pixels per delta: some wheels count lines, or pages (the canvas's width across, its height down).
      const page = e.deltaMode === WheelEvent.DOM_DELTA_PAGE;
      const line = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : 1;
      df.canvas_x -= e.deltaX * (page ? el.clientWidth : line);
      df.canvas_y -= e.deltaY * (page ? el.clientHeight : line);
      df.precanvas.style.transform = `translate(${df.canvas_x}px, ${df.canvas_y}px) scale(${df.zoom})`;
      df.dispatch("translate", { x: df.canvas_x, y: df.canvas_y });
    });
  } else {
    console.error("misag: Drawflow no longer has the fields the wheel scrolls with; the wheel does not scroll the canvas");
  }
  df.on("connectionCreated", (e) => {
    if (rendering) return;
    const p = port(e);
    if (p) events.connected(p[0].id, p[1], p[2].id);
    else dropped("connection", e);
  });
  df.on("connectionRemoved", (e) => {
    if (rendering) return;
    const p = port(e);
    if (p) events.disconnected(p[0].id, p[1], p[2].id);
    else dropped("disconnection", e);
  });
  df.on("nodeMoved", (id) => {
    const s = step(id);
    // Where the step is drawn: Drawflow's own pos_x/pos_y lag behind by the last mouse move.
    const node = el.querySelector<HTMLElement>(`#node-${id}`);
    if (s && node) events.moved(s.id, parseFloat(node.style.left), parseFloat(node.style.top));
    else dropped("move", id);
  });
  df.on("nodeRemoved", (id) => {
    if (rendering) return;
    const s = step(id);
    if (s) events.removed(s.id);
    else dropped("deletion", id);
  });
  df.on("nodeSelected", (id) => {
    selected = String(id);
    events.selected(step(id)?.id ?? null);
  });
  df.on("nodeUnselected", () => {
    selected = null;
    events.selected(null);
  });

  function render(next: Graph) {
    rendering = true;
    try {
      graph = next;
      df.clear();
      stepOf.clear();
      drawnAs.clear();
      selected = null;
      for (const s of graph.nodes) {
        const inputs = s.kind === "start" ? 0 : 1;
        const limit = s.maxRuns === undefined ? "" : `<span class="limit" title="Runs at most ${s.maxRuns} times">≤${s.maxRuns}</span>`;
        const clears = s.clear ? `<span class="clears" title="Starts with /clear">/clear</span>` : "";
        const html = `<div class="kind">${KIND_LABELS[s.kind]}${limit}${clears}</div><div class="title"></div>`;
        const id = df.addNode(s.kind, inputs, s.outputs.length, s.x, s.y, s.kind, {}, html, false);
        stepOf.set(String(id), s);
        drawnAs.set(s.id, id);
        const node = el.querySelector(`#node-${id}`)!;
        // Text, not markup: titles are the user's.
        node.querySelector(".title")!.textContent = s.title;
        s.outputs.forEach((output, i) => {
          const o = node.querySelector<HTMLElement>(`.outputs .output_${i + 1}`)!;
          o.dataset.label = output;
          o.title = output;
        });
      }
      for (const e of graph.edges) {
        const from = graph.nodes.find((s) => s.id === e.from);
        const output = from ? from.outputs.indexOf(e.output) + 1 : 0;
        const [a, b] = [drawnAs.get(e.from), drawnAs.get(e.to)];
        if (a !== undefined && b !== undefined && output > 0) df.addConnection(a, b, `output_${output}`, "input_1");
        else dropped("connection to draw", e);
      }
      mark(marked);
    } finally {
      rendering = false;
    }
  }

  function refresh() {
    for (const id of drawnAs.values()) df.updateConnectionNodes(`node-${id}`);
  }

  function retitle(id: string, title: string) {
    const drawn = drawnAs.get(id);
    const node = drawn === undefined ? null : el.querySelector(`#node-${drawn} .title`);
    if (node) node.textContent = title;
  }

  function mark(id: string | null) {
    marked = id;
    for (const [stepId, drawn] of drawnAs) el.querySelector(`#node-${drawn}`)?.classList.toggle("current", stepId === id);
  }

  return { render, retitle, refresh, mark };
}
