import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { DEFAULT_FONT_SIZE, errorText, FONT_SIZES, type Settings } from "./ipcTypes";
import type { NoticeSink } from "./notices";
import { enablePage, showPageError, type Page } from "./pages";
import { isFinishSound, playSound, type FinishSound } from "./sounds";
import type { Tree } from "./tree";

export interface SettingsPageOptions {
  /** The Settings and Workspace settings pages. */
  settings: Page;
  workspace: Page;
  notices: NoticeSink;
  /** The Workspace box's tree, rooted at the workspace folder. */
  tree: Pick<Tree, "setRoot" | "listedRoot">;
  /** Fits the shown terminal to a new font size; a hidden one measures it again when shown. */
  fitShown(): void;
}

export interface SettingsPage {
  /** The stored settings, as last loaded or saved; the defaults until the first load. */
  current(): Settings;
  /**
   * Fills the page from the stored settings, after the loads and saves queued before.
   * Its form (disabled when its tab opened) is enabled once they are read.
   */
  loadInto(p: Page): void;
  /**
   * Reads the stored settings at startup, then roots the tree at the workspace
   * folder; if they cannot be read it shows home, and says so. Nothing after `signal` aborts.
   */
  start(signal: AbortSignal): void;
  /** Plays the finish sound chosen, unless it is off; if it cannot play, a notice says why, until a sound plays again. */
  playFinish(): void;
}

/** The stored settings, and the Settings and Workspace settings pages that show and change them. */
export function createSettingsPage(opts: SettingsPageOptions): SettingsPage {
  const { notices, tree } = opts;
  const settingsForm = opts.settings.form;
  let settings: Settings = { skipPermissions: false, workspace: null, disableAutoMemory: false, fontSize: DEFAULT_FONT_SIZE, finishSound: "off" };
  // Loads and saves run one after another, so an earlier one cannot finish last and win.
  let settingsQueue = Promise.resolve();
  const queue = (task: () => Promise<void>) => {
    // A task that throws is logged, so it cannot stop the ones queued after it.
    settingsQueue = settingsQueue.then(task).catch((err) => console.error("misag: settings task failed", err));
  };

  function field<T extends HTMLInputElement | HTMLSelectElement = HTMLInputElement>(name: string) {
    return settingsForm.elements.namedItem(name) as T;
  }
  const workspaceInput = opts.workspace.form.elements.namedItem("workspace") as HTMLInputElement;

  /** Shows the stored settings on every page. */
  function fillForms() {
    field("skipPermissions").checked = settings.skipPermissions;
    field<HTMLSelectElement>("disableAutoMemory").value = String(settings.disableAutoMemory ?? false);
    field("fontSize").value = String(settings.fontSize ?? DEFAULT_FONT_SIZE);
    field<HTMLSelectElement>("finishSound").value = settings.finishSound ?? "off";
    workspaceInput.value = settings.workspace ?? "";
  }

  function readForm(): Settings {
    return {
      ...settings,
      skipPermissions: field("skipPermissions").checked,
      disableAutoMemory: field<HTMLSelectElement>("disableAutoMemory").value === "true",
      fontSize: Number(field("fontSize").value),
      finishSound: chosenSound(),
    };
  }

  /** The finish sound chosen on the page; one it does not know keeps the stored one. */
  function chosenSound(): FinishSound {
    const value = field<HTMLSelectElement>("finishSound").value;
    return isFinishSound(value) ? value : (settings.finishSound ?? "off");
  }

  /** Sets the terminals' font size from the settings. */
  function applyFontSize() {
    document.documentElement.style.setProperty("--terminal-font-size", `${settings.fontSize ?? DEFAULT_FONT_SIZE}px`);
    opts.fitShown();
  }

  async function load() {
    settings = await invoke<Settings>("settings_load");
    applyFontSize();
  }

  /** Roots the tree at the workspace folder from the settings (home by default). */
  function showWorkspace() {
    tree.setRoot(settings.workspace ?? "~");
  }

  /**
   * Fills the page from the stored settings, which every launch reads too, so it shows
   * what new tabs will use. Its form (disabled by `Pages.open`) is enabled once they are
   * read, and stays disabled if they cannot be, so nothing is saved over them.
   */
  async function loadPage(p: Page) {
    try {
      await load();
      fillForms();
      showWorkspace();
      showPageError(p, null);
      enablePage(p, true);
    } catch (err) {
      console.error("misag: could not load settings", err);
      showPageError(p, `Settings could not be read (${errorText(err)}), ${p.unreadable}`);
    }
  }

  /** Saves the settings page; if that fails, the form goes back to the stored settings. */
  async function saveForm() {
    const next = readForm();
    const { min, max } = FONT_SIZES;
    if (!Number.isInteger(next.fontSize) || next.fontSize < min || next.fontSize > max) {
      showPageError(opts.settings, `Settings not saved: the font size must be ${min} to ${max}.`);
    } else {
      try {
        const before = settings.finishSound;
        settings = await invoke<Settings>("settings_save", { settings: next });
        showPageError(opts.settings, null);
        applyFontSize();
        // A newly chosen sound plays, so it can be heard before a Claude finishes.
        if (settings.finishSound !== before) playFinish();
      } catch (err) {
        console.error("misag: could not save settings", err);
        showPageError(opts.settings, `Settings not saved: ${errorText(err)}`);
      }
    }
    fillForms();
  }

  /**
   * Plays the finish sound, unless it is off (or missing, from an older backend); if it
   * cannot play, a notice says why, until a sound plays again.
   */
  function playFinish() {
    const sound = settings.finishSound;
    if (!sound || sound === "off") return;
    void playSound(sound, (err) => {
      console.error("misag: could not play the finish sound", err);
      notices.set("finish-sound", `The finish sound could not play (${errorText(err)}). Check your sound output, or turn the sound off in Settings.`);
    }).then((played) => {
      if (played) notices.set("finish-sound", null);
    });
  }

  /** Stores the workspace folder (null for home); a refused folder is shown on the workspace page. */
  function setWorkspace(workspace: string | null) {
    queue(async () => {
      try {
        settings = await invoke<Settings>("settings_save", { settings: { ...settings, workspace } });
        showPageError(opts.workspace, null);
        fillForms();
        showWorkspace();
      } catch (err) {
        console.error("misag: could not save the workspace folder", err);
        showPageError(opts.workspace, `Workspace folder not saved: ${errorText(err)}`);
      }
    });
  }

  async function chooseWorkspace() {
    // The listed root has `~` expanded, which the picker needs.
    const picked = await openDialog({ directory: true, multiple: false, defaultPath: tree.listedRoot() });
    if (typeof picked === "string") setWorkspace(picked);
  }

  const pickWorkspace = () =>
    chooseWorkspace().catch((err) => {
      console.error("misag: could not open the folder picker", err);
      showPageError(opts.workspace, `The folder picker could not be opened: ${errorText(err)}`);
    });

  function start(signal: AbortSignal) {
    queue(async () => {
      if (signal.aborted) return;
      try {
        await load();
      } catch (err) {
        console.error("misag: could not load settings", err);
        if (signal.aborted) return;
        notices.set(
          "settings",
          `Settings could not be read (${errorText(err)}), ${opts.workspace.unreadable} Claude tabs start with permission prompts and auto memory off.`,
        );
      }
      if (!signal.aborted) showWorkspace();
    });
  }

  settingsForm.onchange = () => queue(saveForm);
  // The path field is read-only: clicking it (or Enter or Space on it) opens the folder picker.
  workspaceInput.onclick = pickWorkspace;
  workspaceInput.onkeydown = (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    e.preventDefault();
    pickWorkspace();
  };
  opts.workspace.el.querySelector<HTMLButtonElement>(".home-workspace")!.onclick = () => setWorkspace(null);

  return {
    current: () => settings,
    loadInto: (p) => queue(() => loadPage(p)),
    start,
    playFinish,
  };
}
