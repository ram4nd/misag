import { afterEach, describe, expect, test, vi } from "vitest";
import { createLayoutStore } from "./layoutStore";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => null) }));

// The window is faked: `close` asks it to close, and resolves to whether that was cancelled.
const win = vi.hoisted(() => ({ close: undefined as undefined | (() => Promise<boolean>) }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: async (handler: (event: { preventDefault(): void }) => Promise<void>) => {
      win.close = async () => {
        let prevented = false;
        await handler({ preventDefault: () => (prevented = true) });
        return prevented;
      };
      return () => {};
    },
  }),
}));

afterEach(() => {
  vi.restoreAllMocks();
});

describe("closing the window", () => {
  test("flow editors that fail to flush count as not stored: the first close warns, the next quits", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const notices = { set: vi.fn() };
    const store = createLayoutStore({
      signal: new AbortController().signal,
      notices,
      tabs: () => [],
      active: () => null,
      followClaude: () => {},
      spawn: async () => {},
      activate: () => {},
      flushEditors: () => Promise.reject(new Error("db gone")),
    });
    await store.watchWindowClose();
    expect(await win.close!()).toBe(true);
    expect(notices.set).toHaveBeenCalledWith("flows", expect.stringContaining("Close the window again"), false);
    expect(error).toHaveBeenCalledWith("misag: could not save the flows' changes", expect.any(Error));
    expect(await win.close!()).toBe(false);
  });
});
