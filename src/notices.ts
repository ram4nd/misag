/** The notice bar above the terminals. */
export interface Notices {
  /**
   * Shows `text` under `topic`, or clears the topic with null (or empty text). `key`
   * identifies the underlying event (the text by default): a dismissed notice comes
   * back only when the key changes, not when polling reports the same thing again.
   * Pass a key when the text can change for the same event.
   */
  set(topic: string, text: string | null, dismissible?: boolean, key?: string): void;
  /** Removes every notice about the tab (see `tabTopic`), and what was dismissed of them. */
  clearTab(tab: { tabId: string }): void;
  render(): void;
  /** Hides the dismissible notices until their key changes. */
  dismiss(): void;
}

/** The notice topic for something about one tab; `Notices.clearTab` removes them all. */
export function tabTopic(tab: { tabId: string }, what: string): string {
  return `tab:${tab.tabId}:${what}`;
}

/** What most parts of the app need of the notices: setting and clearing their own. */
export type NoticeSink = Pick<Notices, "set">;

/** The notices shown in `el`, whose span holds their text and whose button dismisses them. */
export function createNotices(el: HTMLElement): Notices {
  /** The notices shown, by topic, so a later update replaces an earlier one. */
  const notices = new Map<string, { text: string; dismissible: boolean; key: string }>();
  /** Per topic, the key of the notice the user dismissed; it stays hidden until the key changes. */
  const dismissed = new Map<string, string>();

  function set(topic: string, text: string | null, dismissible = true, key = text ?? "") {
    if (!text) {
      notices.delete(topic);
      dismissed.delete(topic);
    } else if (dismissed.get(topic) !== key) {
      notices.set(topic, { text, dismissible, key });
    }
    render();
  }

  function clearTab(tab: { tabId: string }) {
    const prefix = tabTopic(tab, "");
    for (const topic of notices.keys()) if (topic.startsWith(prefix)) notices.delete(topic);
    for (const topic of dismissed.keys()) if (topic.startsWith(prefix)) dismissed.delete(topic);
    render();
  }

  function render() {
    el.hidden = notices.size === 0;
    el.querySelector("span")!.textContent = [...notices.values()].map((n) => n.text).join(" ");
    el.querySelector("button")!.hidden = ![...notices.values()].every((n) => n.dismissible);
  }

  function dismiss() {
    for (const [topic, n] of notices) {
      if (!n.dismissible) continue;
      notices.delete(topic);
      dismissed.set(topic, n.key);
    }
    render();
  }

  return { set, clearTab, render, dismiss };
}
