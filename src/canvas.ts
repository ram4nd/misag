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
}

const KIND_LABELS: Record<Step["kind"], string> = { start: "Start", prompt: "Prompt", manual: "Manual", end: "End" };

/**
 * A Drawflow canvas in `el` showing a flow's steps. Drawflow's own node ids stay in
 * here: everything outside uses the flow's step ids.
 */
export function createCanvas(el: HTMLElement, events: CanvasEvents): Canvas {
  const df = new Drawflow(el);
  df.reroute = false;
  let graph: Graph = { version: 1, nodes: [], edges: [] };
  const stepOf = new Map<string, Step>();
  const drawnAs = new Map<string, number>();
  // Drawflow reports what `render` itself draws as if the user did it.
  let rendering = false;

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
  const dropped = (what: string, detail: unknown) => console.error(`raterm: ignored a canvas ${what}`, detail);
  df.start();
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
        const html = `<div class="kind">${KIND_LABELS[s.kind]}</div><div class="title"></div>`;
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

  return { render, retitle, refresh };
}
