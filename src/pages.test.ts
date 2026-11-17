import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createPages, staticPage, type Page, type Pages, type StaticPageId } from "./pages";

/** The flow editors created, each with a `flush` and `load` a test can replace. */
const editors = vi.hoisted(() => ({
  all: [] as { el: HTMLElement; flush: () => Promise<boolean>; load: () => Promise<boolean> }[],
}));
vi.mock("./flowEditor", () => ({
  createFlowEditor: () => {
    const el = document.createElement("div");
    el.innerHTML = `<form><p class="error" hidden></p><fieldset></fieldset></form>`;
    const editor = { el, flush: async () => true, load: async () => true, shown: () => {}, discard: () => {}, showRun: () => {} };
    editors.all.push(editor);
    return editor;
  },
}));

/** A page's element with a form, its error line and one fieldset. */
function pageEl(): HTMLElement {
  const el = document.createElement("section");
  el.innerHTML = `<form><p class="error" hidden></p><fieldset></fieldset></form>`;
  return el;
}
const errorOf = (p: Page) => p.form.querySelector<HTMLElement>(".error")!;
const flush = () => new Promise((r) => setTimeout(r, 0));

let pages: Record<StaticPageId, Page>;
let ui: Pages;

beforeEach(() => {
  editors.all = [];
  pages = {
    settings: staticPage("Settings", "settings", "", pageEl()),
    workspace: staticPage("Workspace settings", "folder-cog", "", pageEl()),
    flows: staticPage("Flows", "workflow", "", pageEl()),
  };
  ui = createPages({
    pages,
    terms: document.createElement("div"),
    runs: { proceed: async () => null, resume: async () => null, show: () => {} },
    render: () => {},
    showActive: () => {},
    showTerminal: () => {},
    loadSettings: () => {},
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("pages", () => {
  test("a page whose load fails says so and stays disabled", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    pages.flows.load = () => Promise.reject(new Error("broken"));
    ui.open("flows");
    await flush();
    expect(errorOf(pages.flows).hidden).toBe(false);
    expect(errorOf(pages.flows).textContent).toContain("broken");
    expect(pages.flows.el.querySelector("fieldset")!.disabled).toBe(true);
    expect(error).toHaveBeenCalledWith("misag: could not load the page", expect.any(Error));
  });

  test("a flow editor whose changes cannot be flushed stays open, shown, and says why", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    ui.openFlowEditor({ id: 1, name: "Deploy", archived: false } as Parameters<Pages["openFlowEditor"]>[0]);
    ui.open("settings");
    editors.all[0].flush = () => Promise.reject(new Error("db gone"));
    ui.close("flow:1");
    await flush();
    expect(ui.shown()).toBe("flow:1");
    expect([...ui.editors()].map(([id]) => id)).toEqual([1]);
    const editorPage = editors.all[0].el.querySelector<HTMLElement>(".error")!;
    expect(editorPage.hidden).toBe(false);
    expect(editorPage.textContent).toContain("db gone");
    expect(error).toHaveBeenCalledWith("misag: could not save the flow's changes", expect.any(Error));
  });
});
