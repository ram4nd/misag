// boot() in main.ts builds the app's parts one after another and hands each the ones
// built later through arrow functions, which throw if called before those exist. So no
// factory may call what it is given while it is being built: this builds each one with
// every dependency it would call throwing.
import { beforeEach, expect, test, vi } from "vitest";
import indexHtml from "../index.html?raw";
import { createFlows } from "./flows";
import { createHistory } from "./history";
import { createLayoutStore } from "./layoutStore";
import { createPages, staticPage, type Page, type StaticPageId } from "./pages";
import { createPathTyping } from "./pathDrops";
import { createRunController } from "./runController";
import { createSettingsPage } from "./settingsPage";
import { createSidebar } from "./sidebar";
import { createTabPoller } from "./tabPoller";
import { createTabs } from "./tabs";
import type { Session } from "./tabState";
import { createTree } from "./tree";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: () => {
    throw new Error("invoked during construction");
  },
  Channel: class {},
}));

/** The dependencies called while the factories were built, including from async code they started. */
let called: string[];

/**
 * `given` (the elements, signal and plain values a factory reads while it is built),
 * with every other property a function that throws when called, and nested
 * dependencies (`notices`, `runs`, …) built the same way.
 */
function strict<T>(given: object = {}): T {
  return new Proxy(given, {
    get(target, key) {
      if (key in target) return (target as Record<PropertyKey, unknown>)[key];
      // Not a thenable, so awaiting it would not call `then`.
      if (key === "then") return undefined;
      const call = () => {
        called.push(String(key));
        throw new Error(`called ${String(key)} during construction`);
      };
      return new Proxy(call, { get: (_, nested) => (nested === "then" ? undefined : strict()) });
    },
  }) as T;
}

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

beforeEach(() => {
  called = [];
  document.body.innerHTML = new DOMParser().parseFromString(indexHtml, "text/html").body.innerHTML;
});

test("no factory boot() wires calls its dependencies while it is built", async () => {
  const signal = new AbortController().signal;
  const pages: Record<StaticPageId, Page> = {
    settings: staticPage("Settings", "settings", "", $("#settings")),
    workspace: staticPage("Workspace settings", "folder-cog", "", $("#workspace-settings")),
    flows: staticPage("Flows", "workflow", "", $("#flows-settings")),
  };
  const build = [
    () => createTree($<HTMLUListElement>("#workspace-box .tree"), strict()),
    () => createSettingsPage(strict({ settings: pages.settings, workspace: pages.workspace })),
    () => createSidebar(strict({ sidebar: $("#sidebar"), resizer: $("#sidebar-resizer"), signal })),
    () => createFlows(strict({ box: $("#flows-box"), page: pages.flows.el })),
    () => createRunController<Session>(strict()),
    () => createPages(strict({ pages, terms: $("#terminals") })),
    () => createTabs(strict({ signal, bar: $("#tabs"), terms: $("#terminals") })),
    () => createTabPoller(strict({ signal })),
    () => createLayoutStore(strict({ signal })),
    () => createPathTyping(strict()),
    // The Session history box reads the sessions at once, so boot() builds it after what its `list` reaches.
    () => createHistory(strict({ box: $("#history-box"), list: async () => ({ sessions: [], more: false, unreadable: 0 }) })),
  ];
  for (const factory of build) expect(factory).not.toThrow();
  await new Promise((r) => setTimeout(r, 0));
  expect(called).toEqual([]);
});
