import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

/** A stand-in AudioContext that records the oscillators started on it; jsdom has no Web Audio. */
class FakeContext {
  static made: FakeContext[] = [];
  /** Makes the next constructor call throw, as a webview without Web Audio would. */
  static failNext: Error | null = null;
  state: AudioContextState = "suspended";
  currentTime = 5;
  started: { freq: number; at: number; stop: number }[] = [];
  resumeResult: Promise<void> = Promise.resolve();
  destination = {};
  constructor() {
    if (FakeContext.failNext) {
      const err = FakeContext.failNext;
      FakeContext.failNext = null;
      throw err;
    }
    FakeContext.made.push(this);
  }
  resume() {
    return this.resumeResult.then(() => void (this.state = "running"));
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

afterEach(() => vi.unstubAllGlobals());

describe("sounds", () => {
  test.each([
    ["chime", 2],
    ["bell", 2],
    ["pop", 1],
  ] as const)("%s plays %i tones from now, each stopped after it starts", (name, tones) => {
    const onFail = vi.fn();
    sounds.playSound(name, onFail);
    const [context] = FakeContext.made;
    expect(context.started).toHaveLength(tones);
    for (const tone of context.started) {
      expect(tone.at).toBeGreaterThanOrEqual(5);
      expect(tone.stop).toBeGreaterThan(tone.at);
      expect(tone.freq).toBeGreaterThan(0);
    }
    expect(onFail).not.toHaveBeenCalled();
  });

  test("one context is reused, and a closed one is replaced", () => {
    sounds.playSound("pop", vi.fn());
    sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(1);
    FakeContext.made[0].state = "closed";
    sounds.playSound("pop", vi.fn());
    expect(FakeContext.made).toHaveLength(2);
    expect(FakeContext.made[1].started).toHaveLength(1);
  });

  test("a context that cannot be made reports why, and the next play tries again", () => {
    FakeContext.failNext = new Error("no Web Audio");
    const onFail = vi.fn();
    sounds.playSound("bell", onFail);
    expect(onFail).toHaveBeenCalledWith(new Error("no Web Audio"));
    sounds.playSound("bell", onFail);
    expect(FakeContext.made).toHaveLength(1);
    expect(onFail).toHaveBeenCalledTimes(1);
  });

  test("a context that cannot resume reports why", async () => {
    sounds.primeAudio();
    FakeContext.made[0].resumeResult = Promise.reject(new Error("no device"));
    const onFail = vi.fn();
    sounds.playSound("chime", onFail);
    await vi.waitFor(() => expect(onFail).toHaveBeenCalledWith(new Error("no device")));
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
