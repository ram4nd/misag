import { beforeEach, describe, expect, test } from "vitest";
import { createNotices, tabTopic, type Notices } from "./notices";

let el: HTMLDivElement;
let notices: Notices;

const text = () => el.querySelector("span")!.textContent;
const button = () => el.querySelector("button")!;

beforeEach(() => {
  el = document.createElement("div");
  el.append(document.createElement("span"), document.createElement("button"));
  notices = createNotices(el);
  notices.render();
});

describe("notices", () => {
  test("start hidden, and show each topic's latest text", () => {
    expect(el.hidden).toBe(true);
    notices.set("a", "One.");
    notices.set("b", "Two.");
    notices.set("a", "Three.");
    expect(el.hidden).toBe(false);
    expect(text()).toBe("Three. Two.");
  });

  test("a dismissed notice stays hidden while its key is the same, and comes back with a new one", () => {
    notices.set("hook", "Hook failed.", true, "1");
    notices.dismiss();
    expect(el.hidden).toBe(true);
    // Polling reports the same event again, in other words.
    notices.set("hook", "Hook failed again.", true, "1");
    expect(el.hidden).toBe(true);
    notices.set("hook", "Hook failed.", true, "2");
    expect(text()).toBe("Hook failed.");
  });

  test("without a key, the text is the key", () => {
    notices.set("save", "Could not save.");
    notices.dismiss();
    notices.set("save", "Could not save.");
    expect(el.hidden).toBe(true);
    notices.set("save", "Could not save: disk full.");
    expect(text()).toBe("Could not save: disk full.");
  });

  test("a notice that cannot be dismissed survives dismiss, and hides the dismiss button", () => {
    notices.set("load", "Not saving.", false);
    notices.set("save", "Could not save.");
    expect(button().hidden).toBe(true);
    notices.dismiss();
    expect(text()).toBe("Not saving.");
    expect(button().hidden).toBe(true);
    notices.set("load", null);
    notices.set("save", "Could not save again.");
    expect(button().hidden).toBe(false);
  });

  test("clearing a topic forgets its dismissal, so the same notice shows again", () => {
    notices.set("save", "Could not save.");
    notices.dismiss();
    notices.set("save", null);
    notices.set("save", "Could not save.");
    expect(text()).toBe("Could not save.");
  });

  test("clearTab removes only that tab's notices and dismissals", () => {
    const one = { tabId: "1" };
    const ten = { tabId: "10" };
    notices.set(tabTopic(one, "hook"), "One's hook.");
    notices.set(tabTopic(one, "resume"), "One's resume.");
    notices.dismiss();
    notices.set(tabTopic(ten, "hook"), "Ten's hook.");
    notices.clearTab(one);
    expect(text()).toBe("Ten's hook.");
    // One's dismissals went with it.
    notices.set(tabTopic(one, "hook"), "One's hook.");
    expect(text()).toBe("Ten's hook. One's hook.");
  });
});
