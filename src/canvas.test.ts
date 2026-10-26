import Drawflow from "drawflow";
import { describe, expect, test, vi } from "vitest";
import { createCanvas } from "./canvas";

function setup() {
  const el = document.createElement("div");
  el.className = "canvas";
  document.body.append(el);
  createCanvas(el, { moved() {}, connected() {}, disconnected() {}, removed() {}, selected() {} });
  const drawn = () => el.querySelector<HTMLElement>(".drawflow")!;
  return { el, transform: () => drawn().style.transform };
}

describe("canvas", () => {
  test("the mouse wheel scrolls a flow longer than the canvas", () => {
    const { el, transform } = setup();
    el.querySelector(".drawflow")!.dispatchEvent(new WheelEvent("wheel", { deltaX: 10, deltaY: 300, bubbles: true, cancelable: true }));
    expect(transform()).toBe("translate(-10px, -300px) scale(1)");
  });

  test("a wheel that counts lines or pages scrolls as far as one that counts pixels", () => {
    const { el, transform } = setup();
    const drawn = el.querySelector(".drawflow")!;
    drawn.dispatchEvent(new WheelEvent("wheel", { deltaY: 3, deltaMode: WheelEvent.DOM_DELTA_LINE, bubbles: true, cancelable: true }));
    expect(transform()).toBe("translate(0px, -48px) scale(1)");
  });

  test("a wheel that counts pages scrolls a canvas width across and a canvas height down", () => {
    const { el, transform } = setup();
    const drawn = el.querySelector<HTMLElement>(".drawflow")!;
    const parent = drawn.parentElement!;
    Object.defineProperty(parent, "clientWidth", { value: 800 });
    Object.defineProperty(parent, "clientHeight", { value: 600 });
    drawn.dispatchEvent(new WheelEvent("wheel", { deltaX: 1, deltaY: 1, deltaMode: WheelEvent.DOM_DELTA_PAGE, bubbles: true, cancelable: true }));
    expect(transform()).toBe("translate(-800px, -600px) scale(1)");
  });

  test("a Drawflow without the fields the wheel scrolls with says so, and the wheel is left alone", () => {
    const start = vi.spyOn(Drawflow.prototype, "start").mockImplementation(function () {});
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const el = document.createElement("div");
      createCanvas(el, { moved() {}, connected() {}, disconnected() {}, removed() {}, selected() {} });
      const wheel = new WheelEvent("wheel", { deltaY: 100, bubbles: true, cancelable: true });
      el.firstElementChild!.dispatchEvent(wheel);
      expect(wheel.defaultPrevented).toBe(false);
      expect(error).toHaveBeenCalledWith(expect.stringContaining("the wheel does not scroll the canvas"));
    } finally {
      start.mockRestore();
      error.mockRestore();
    }
  });

  test("the wheel keeps the page from scrolling too, and adds up", () => {
    const { el, transform } = setup();
    const drawn = el.querySelector(".drawflow")!;
    const wheel = new WheelEvent("wheel", { deltaY: 100, bubbles: true, cancelable: true });
    drawn.dispatchEvent(wheel);
    drawn.dispatchEvent(new WheelEvent("wheel", { deltaY: 100, bubbles: true, cancelable: true }));
    expect(wheel.defaultPrevented).toBe(true);
    expect(transform()).toBe("translate(0px, -200px) scale(1)");
  });

  test("Ctrl+wheel zooms rather than scrolls, and scrolling keeps the zoom", () => {
    const { el, transform } = setup();
    const drawn = el.querySelector(".drawflow")!;
    drawn.dispatchEvent(new WheelEvent("wheel", { deltaY: 100, ctrlKey: true, bubbles: true, cancelable: true }));
    expect(transform()).toBe("translate(0px, 0px) scale(0.9)");
    drawn.dispatchEvent(new WheelEvent("wheel", { deltaY: 100, bubbles: true, cancelable: true }));
    expect(transform()).toBe("translate(0px, -100px) scale(0.9)");
  });

  test("dragging anywhere on the empty canvas moves it, also where the drawn area has moved away from", () => {
    const { el, transform } = setup();
    const drag = (target: Element, dy: number) => {
      target.dispatchEvent(new MouseEvent("mousedown", { clientX: 0, clientY: 500, bubbles: true }));
      target.dispatchEvent(new MouseEvent("mousemove", { clientX: 0, clientY: 500 + dy, bubbles: true }));
      target.dispatchEvent(new MouseEvent("mouseup", { clientX: 0, clientY: 500 + dy, bubbles: true }));
    };
    drag(el.querySelector(".drawflow")!, -400);
    // The drawn area has moved up, so the bottom of the canvas is the container itself.
    drag(el.querySelector(".drawflow")!.parentElement!, -400);
    expect(transform()).toBe("translate(0px, -800px) scale(1)");
  });
});
