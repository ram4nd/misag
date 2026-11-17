import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

/** A stand-in AudioContext that records the oscillators started on it; jsdom has no Web Audio. */
class FakeContext {
  static made: FakeContext[] = [];
  /** Makes the next constructor call throw, as a webview without Web Audio would. */
  static failNext: Error | null = null;
  /** As WebKit reports it, which adds "interrupted" (such as after sleep or a change of output device). */
  state: AudioContextState | "interrupted" = "suspended";
  currentTime = 5;
  started: { freq: number; at: number; stop: number }[] = [];
  resumeResult: Promise<void> = Promise.resolve();
  /** What close returns; a close also cancels the tones scheduled on it. */
  closeResult: Promise<void> = Promise.resolve();
  closed = false;
  destination = {};
  listeners: (() => void)[] = [];
  constructor() {
    if (FakeContext.failNext) {
      const err = FakeContext.failNext;
      FakeContext.failNext = null;
      throw err;
    }
    FakeContext.made.push(this);
  }
  stuck = false;
  resume() {
    return this.resumeResult.then(() => {
      if (!this.stuck) this.state = "running";
    });
  }
  /** Makes resume never settle, and one already asked never start it, as a context WebKit left stuck does. */
  stick() {
    this.stuck = true;
    this.resumeResult = new Promise(() => {});
  }
  close() {
    this.closed = true;
    this.started = [];
    this.change("closed");
    return this.closeResult;
  }
  addEventListener(type: string, listener: () => void) {
    if (type === "statechange") this.listeners.push(listener);
  }
  removeEventListener(type: string, listener: () => void) {
    if (type === "statechange") this.listeners = this.listeners.filter((l) => l !== listener);
  }
  /** Changes the state as the browser would, telling the listeners. */
  change(state: FakeContext["state"]) {
    this.state = state;
    for (const listener of [...this.listeners]) listener();
  }
  createGain() {
    const ramp = () => {};
    return { gain: { setValueAtTime: ramp, exponentialRampToValueAtTime: ramp }, connect: (to: unknown) => to };
  }
  createOscillator() {
    const tone = { freq: 0, at: -1, stop: -1 };
    return {
      type: "sine",
      frequency: {
        set value(v: number) {
          tone.freq = v;
        },
      },
      connect: (to: unknown) => to,
      start: (at: number) => {
        tone.at = at;
        this.started.push(tone);
      },
      stop: (at: number) => void (tone.stop = at),
    };
  }
}

let sounds: typeof import("./sounds");

beforeEach(async () => {
  FakeContext.made = [];
  FakeContext.failNext = null;
  vi.stubGlobal("AudioContext", FakeContext);
  vi.resetModules();
  sounds = await import("./sounds");
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("sounds", () => {
  test.each([
    ["chime", 2],
    ["bell", 2],
    ["pop", 1],
  ] as const)("%s plays %i tones from now, each stopped after it starts", async (name, tones) => {
    const onFail = vi.fn();
    expect(await sounds.playSound(name, onFail)).toBe(true);
    const [context] = FakeContext.made;
    expect(context.started).toHaveLength(tones);
    for (const tone of context.started) {
      expect(tone.at).toBeGreaterThanOrEqual(5);
      expect(tone.stop).toBeGreaterThan(tone.at);
      expect(tone.freq).toBeGreaterThan(0);
    }
    expect(onFail).not.toHaveBeenCalled();
  });

  test("one context is reused, and a closed one is replaced", async () => {
    await sounds.playSound("pop", vi.fn());
    await sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(1);
    FakeContext.made[0].state = "closed";
    await sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(2);
    expect(FakeContext.made[1].started).toHaveLength(1);
  });

  test("a context that cannot be made reports why, and the next play tries again", async () => {
    FakeContext.failNext = new Error("no Web Audio");
    const onFail = vi.fn();
    await sounds.playSound("bell", onFail);
    expect(onFail).toHaveBeenCalledWith(new Error("no Web Audio"));
    await sounds.playSound("bell", onFail);
    expect(FakeContext.made).toHaveLength(1);
    expect(onFail).toHaveBeenCalledTimes(1);
  });

  test("a context that cannot resume is replaced, and the sound plays on the new one", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    sounds.primeAudio();
    const [old] = FakeContext.made;
    old.resumeResult = Promise.reject(new Error("no device"));
    const onFail = vi.fn();
    expect(await sounds.playSound("chime", onFail)).toBe(true);
    expect(old.closed).toBe(true);
    expect(old.started).toHaveLength(0);
    expect(FakeContext.made[1].started).toHaveLength(2);
    expect(onFail).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
  });

  test("an output that starts slowly still plays on the same context, without a replacement", async () => {
    vi.useFakeTimers();
    sounds.primeAudio();
    const [slow] = FakeContext.made;
    slow.resumeResult = new Promise((resolve) => setTimeout(resolve, 1000));
    const onFail = vi.fn();
    const played = sounds.playSound("bell", onFail);
    await vi.advanceTimersByTimeAsync(0);
    // Scheduled at once on its clock, which plays them once it runs.
    expect(slow.started).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(await played).toBe(true);
    expect(FakeContext.made).toHaveLength(1);
    expect(slow.closed).toBe(false);
    expect(onFail).not.toHaveBeenCalled();
  });

  test("a context stuck past the long wait is replaced and closed, and the sound plays on the new one", async () => {
    vi.useFakeTimers();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    sounds.primeAudio();
    const [stuck] = FakeContext.made;
    stuck.stick();
    const onFail = vi.fn();
    const played = sounds.playSound("bell", onFail);
    await vi.advanceTimersByTimeAsync(2900);
    expect(FakeContext.made).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(200);
    expect(await played).toBe(true);
    expect(stuck.closed).toBe(true);
    expect(stuck.started).toHaveLength(0);
    expect(FakeContext.made[1].started).toHaveLength(2);
    expect(onFail).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalledTimes(1);
    expect(String(warn.mock.calls[0][0])).toMatch(/suspended after \d+ ms/);
    await sounds.playSound("pop", onFail);
    expect(FakeContext.made).toHaveLength(2);
    expect(FakeContext.made[1].started).toHaveLength(3);
  });

  test("a resume that settles while the context is not running counts as not started", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    vi.spyOn(FakeContext.prototype, "resume").mockImplementationOnce(() => Promise.resolve());
    const onFail = vi.fn();
    expect(await sounds.playSound("pop", onFail)).toBe(true);
    const [old] = FakeContext.made;
    expect(old.closed).toBe(true);
    expect(FakeContext.made[1].started).toHaveLength(1);
    expect(onFail).not.toHaveBeenCalled();
  });

  test("an interrupted context is replaced and closed without waiting on it", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    await sounds.playSound("pop", vi.fn());
    const [old] = FakeContext.made;
    old.state = "interrupted";
    old.stick();
    const onFail = vi.fn();
    expect(await sounds.playSound("pop", onFail)).toBe(true);
    expect(FakeContext.made).toHaveLength(2);
    expect(old.closed).toBe(true);
    expect(FakeContext.made[1].started).toHaveLength(1);
    expect(onFail).not.toHaveBeenCalled();
  });

  test("an interrupted context that runs again on its own is kept", async () => {
    await sounds.playSound("pop", vi.fn());
    const [ctx] = FakeContext.made;
    ctx.change("interrupted");
    ctx.change("running");
    await sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(1);
    expect(ctx.started).toHaveLength(2);
  });

  test("a closed context is dropped, but its late statechange does not drop the newer one", async () => {
    await sounds.playSound("pop", vi.fn());
    const [old] = FakeContext.made;
    old.state = "closed";
    await sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(2);
    old.change("closed");
    await sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(2);
    expect(FakeContext.made[1].started).toHaveLength(2);
  });

  test("two sounds on a stuck context make one replacement, and both play on it", async () => {
    vi.useFakeTimers();
    vi.spyOn(console, "warn").mockImplementation(() => {});
    sounds.primeAudio();
    FakeContext.made[0].stick();
    const onFail = vi.fn();
    const first = sounds.playSound("pop", onFail);
    await vi.advanceTimersByTimeAsync(1000);
    const second = sounds.playSound("pop", onFail);
    await vi.advanceTimersByTimeAsync(5000);
    expect(await first).toBe(true);
    expect(await second).toBe(true);
    expect(FakeContext.made).toHaveLength(2);
    expect(FakeContext.made[0].started).toHaveLength(0);
    expect(FakeContext.made[1].started).toHaveLength(2);
    expect(onFail).not.toHaveBeenCalled();
  });

  test("when the new context does not start either, it reports both states once and leaves no tones", async () => {
    vi.useFakeTimers();
    vi.spyOn(console, "warn").mockImplementation(() => {});
    sounds.primeAudio();
    FakeContext.made[0].stick();
    const realResume = FakeContext.prototype.resume;
    vi.spyOn(FakeContext.prototype, "resume").mockImplementation(function (this: FakeContext) {
      this.stick();
      return realResume.call(this);
    });
    const onFail = vi.fn();
    const played = sounds.playSound("chime", onFail);
    await vi.advanceTimersByTimeAsync(10000);
    expect(await played).toBe(false);
    expect(onFail).toHaveBeenCalledTimes(1);
    const message = String(onFail.mock.calls[0][0]);
    expect(message).toMatch(/sound output did not start/);
    expect(message).toMatch(/old one was suspended.*new one suspended/);
    expect(FakeContext.made).toHaveLength(2);
    for (const context of FakeContext.made) expect(context.started).toHaveLength(0);
  });

  test("a replacement that cannot be made reports why once", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    await sounds.playSound("pop", vi.fn());
    FakeContext.made[0].state = "interrupted";
    FakeContext.failNext = new Error("no Web Audio");
    const onFail = vi.fn();
    expect(await sounds.playSound("pop", onFail)).toBe(false);
    expect(onFail).toHaveBeenCalledTimes(1);
    expect(onFail).toHaveBeenCalledWith(new Error("no Web Audio"));
  });

  test("a close that fails is logged", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    await sounds.playSound("pop", vi.fn());
    const [old] = FakeContext.made;
    const failure = new Error("already closing");
    old.closeResult = Promise.reject(failure);
    old.state = "interrupted";
    await sounds.playSound("pop", vi.fn());
    await vi.waitFor(() => expect(warn.mock.calls.some((call) => call.includes(failure))).toBe(true));
  });

  test("an onFail that throws is logged once, and the sound resolves false", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    FakeContext.failNext = new Error("no Web Audio");
    const onFail = vi.fn(() => {
      throw new Error("notice broke");
    });
    expect(await sounds.playSound("pop", onFail)).toBe(false);
    expect(onFail).toHaveBeenCalledTimes(1);
    expect(error).toHaveBeenCalledTimes(1);
  });

  test("priming replaces an interrupted or closed context", async () => {
    sounds.primeAudio();
    FakeContext.made[0].state = "interrupted";
    sounds.primeAudio();
    expect(FakeContext.made).toHaveLength(2);
    await vi.waitFor(() => expect(FakeContext.made[1].state).toBe("running"));
    FakeContext.made[1].state = "closed";
    sounds.primeAudio();
    expect(FakeContext.made).toHaveLength(3);
  });

  test("priming logs a resume that fails", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    sounds.primeAudio();
    const failure = new Error("no device");
    FakeContext.made[0].resumeResult = Promise.reject(failure);
    sounds.primeAudio();
    await vi.waitFor(() => expect(warn.mock.calls.some((call) => call.includes(failure))).toBe(true));
  });

  test("priming makes and resumes the context, and does not fail without Web Audio", async () => {
    sounds.primeAudio();
    expect(FakeContext.made).toHaveLength(1);
    await vi.waitFor(() => expect(FakeContext.made[0].state).toBe("running"));
    sounds.primeAudio();
    expect(FakeContext.made).toHaveLength(1);
    vi.stubGlobal("AudioContext", undefined);
    vi.resetModules();
    const fresh = await import("./sounds");
    expect(() => fresh.primeAudio()).not.toThrow();
  });

  test("only the known finish sounds are accepted", () => {
    expect(sounds.FINISH_SOUNDS).toEqual(["off", "chime", "bell", "pop"]);
    expect(sounds.isFinishSound("bell")).toBe(true);
    expect(sounds.isFinishSound("siren")).toBe(false);
    expect(sounds.isFinishSound(undefined)).toBe(false);
  });
});
