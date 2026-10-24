// The part of Drawflow (which ships no types) that src/canvas.ts uses.
declare module "drawflow" {
  export interface ConnectionEvent {
    output_id: number | string;
    input_id: number | string;
    output_class: string;
    input_class: string;
  }
  export default class Drawflow {
    constructor(container: HTMLElement);
    reroute: boolean;
    start(): void;
    clear(): void;
    addNode(
      name: string,
      inputs: number,
      outputs: number,
      x: number,
      y: number,
      className: string,
      data: object,
      html: string,
      typenode: boolean,
    ): number;
    addConnection(output: number, input: number, outputClass: string, inputClass: string): void;
    updateConnectionNodes(id: string): void;
    on(event: "connectionCreated" | "connectionRemoved", callback: (e: ConnectionEvent) => void): void;
    on(event: "nodeMoved" | "nodeRemoved" | "nodeSelected", callback: (id: number | string) => void): void;
    on(event: "nodeUnselected", callback: () => void): void;
  }
}
