/** The sounds that can play when Claude in a tab finishes: mirrors `settings::FinishSound`. */
export const FINISH_SOUNDS = ["off", "chime", "bell", "pop"] as const;
export type FinishSound = (typeof FINISH_SOUNDS)[number];

/** A sound that plays. */
export type Sound = Exclude<FinishSound, "off">;

/** Whether `value` is a finish sound this version knows. */
export function isFinishSound(value: unknown): value is FinishSound {
  return (FINISH_SOUNDS as readonly unknown[]).includes(value);
}

/** A tone: its frequency (Hz), when it starts and how long it rings (s, above 0), and its loudness (0 to 1). */
type Tone = { freq: number; at: number; length: number; gain: number; wave?: OscillatorType };

/** Each sound as tones, made with Web Audio so no sound files ship with the app. */
const SOUNDS: Record<Sound, Tone[]> = {
  chime: [
    { freq: 880, at: 0, length: 0.5, gain: 0.25 },
    { freq: 1318.5, at: 0.12, length: 0.7, gain: 0.2 },
  ],
  bell: [
    { freq: 660, at: 0, length: 1.2, gain: 0.25 },
    { freq: 1650, at: 0, length: 0.6, gain: 0.08 },
  ],
  pop: [{ freq: 520, at: 0, length: 0.12, gain: 0.3, wave: "triangle" }],
};

/**
 * How long a context gets to start running before it is replaced (ms). Outputs
 * can be slow to start (Bluetooth after wake, GStreamer's first start on Linux),
 * and tones scheduled on a context still play once it runs, so this is generous.
 */
const START_WAIT_MS = 3000;

/** The one audio context for the page, or null until a sound or a key makes one. */
let context: AudioContext | null = null;

/**
 * Whether the context cannot play now: closed, or "interrupted", a state only
 * WebKit has (such as after sleep or a change of output device). WebKit may end
 * an interruption on its own, so only a closed context is dropped as it happens;
 * an interrupted one is replaced when a sound or a key needs it.
 */
function stopped(ctx: AudioContext): boolean {
  const state: string = ctx.state;
  return state === "closed" || state === "interrupted";
}

/** A new audio context, which stops being the page's context once it closes, so the next sound makes another. */
function makeContext(): AudioContext {
  const ctx = new AudioContext();
  ctx.addEventListener?.("statechange", () => {
    if (context === ctx && ctx.state === "closed") context = null;
  });
  return ctx;
}

/** The audio context, made on first use and again if the old one closed. */
function audio(): AudioContext {
  if (context?.state === "closed") retire(context);
  if (!context) context = makeContext();
  return context;
}

/** Lets go of the context: it stops being the page's, and is closed, which cancels the tones scheduled on it. */
function retire(ctx: AudioContext) {
  if (context === ctx) context = null;
  if (ctx.state === "closed") return;
  const failed = (err: unknown) => console.warn("misag: could not close the old sound output", err);
  try {
    ctx.close().catch(failed);
  } catch (err) {
    failed(err);
  }
}

/** Whether a context started, and the error its resume gave, if any. */
type Start = { running: boolean; err?: unknown };

/**
 * Resumes the context and says whether it runs within `START_WAIT_MS`; on macOS a
 * context may stay suspended with a resume that never settles. A resume that
 * settles while the context is not running counts as not started.
 */
function started(ctx: AudioContext): Promise<Start> {
  if (ctx.state === "running") return Promise.resolve({ running: true });
  if (stopped(ctx)) return Promise.resolve({ running: false });
  return new Promise((resolve) => {
    let settled = false;
    const done = (result: Start) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      ctx.removeEventListener?.("statechange", changed);
      resolve(result);
    };
    const changed = () => {
      if (ctx.state === "running") done({ running: true });
      else if (stopped(ctx)) done({ running: false });
    };
    const timer = setTimeout(() => done({ running: false }), START_WAIT_MS);
    ctx.addEventListener?.("statechange", changed);
    try {
      ctx.resume().then(
        () => done({ running: ctx.state === "running" }),
        (err) => done({ running: false, err }),
      );
    } catch (err) {
      done({ running: false, err });
    }
  });
}

/** Schedules the sound's tones on the context's clock, which plays them once it runs. */
function schedule(ctx: AudioContext, name: Sound) {
  const now = ctx.currentTime;
  for (const tone of SOUNDS[name]) {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = tone.wave ?? "sine";
    osc.frequency.value = tone.freq;
    const start = now + tone.at;
    gain.gain.setValueAtTime(0.0001, start);
    gain.gain.exponentialRampToValueAtTime(tone.gain, start + 0.01);
    gain.gain.exponentialRampToValueAtTime(0.0001, start + tone.length);
    osc.connect(gain).connect(ctx.destination);
    osc.start(start);
    osc.stop(start + tone.length);
  }
}

/**
 * The context to use in place of `old`: the page's, if another sound already
 * replaced `old`, or else a new one, after `old` is closed (cancelling its tones).
 */
function replacement(old: AudioContext, waited: number): AudioContext {
  if (context && context !== old && !stopped(context)) return context;
  console.warn(`misag: the sound output was ${old.state}${waited ? ` after ${waited} ms` : ""}, so a new one is started`);
  retire(old);
  return audio();
}

/** The message for an error, or a string for anything else. */
function text(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** Plays the sound, resolving once it runs; throws why it cannot. */
async function play(name: Sound): Promise<void> {
  const ctx = audio();
  let start: Start = { running: false };
  let waited = 0;
  if (!stopped(ctx)) {
    schedule(ctx, name);
    const since = Date.now();
    start = await started(ctx);
    if (start.running) return;
    waited = Date.now() - since;
  }
  const oldState = ctx.state;
  const fresh = replacement(ctx, waited);
  schedule(fresh, name);
  const again = await started(fresh);
  if (again.running) return;
  const freshState = fresh.state;
  // Its tones would otherwise play whenever it starts, long after the notice.
  retire(fresh);
  const cause = again.err ?? start.err;
  const detail = cause === undefined ? "" : `: ${text(cause)}`;
  throw new Error(`the sound output did not start (the old one was ${oldState}, the new one ${freshState})${detail}`, { cause });
}

/**
 * Makes the audio context and resumes it, replacing one that is closed or
 * interrupted. Called on a key or click: WebKitGTK keeps a context started
 * without one suspended, so a sound played later from a timer would not be
 * heard. (On macOS wry lets audio play without one, so this matters mostly on Linux.)
 */
export function primeAudio() {
  try {
    let ctx = audio();
    if (stopped(ctx)) {
      retire(ctx);
      ctx = audio();
    }
    ctx.resume().catch((err) => console.warn("misag: could not start the sound output", err));
  } catch {
    // playSound reports it, when a sound is due.
  }
}

/**
 * Plays the sound. Its tones are scheduled on the audio context at once and play
 * when it runs. A context that is closed or interrupted, or still not running
 * after `START_WAIT_MS`, is closed and replaced by a new one, so a sound may be
 * up to that late; if the new one does not run within as long either, or Web
 * Audio is missing or fails, calls `onFail` once with why and plays nothing.
 * Resolves true once the sound is scheduled on a running context; never rejects,
 * so callers need not wait on it.
 */
export async function playSound(name: Sound, onFail: (err: unknown) => void): Promise<boolean> {
  let failure: unknown;
  try {
    await play(name);
    return true;
  } catch (err) {
    failure = err;
  }
  try {
    onFail(failure);
  } catch (err) {
    console.error("misag: could not report a sound that did not play", err);
  }
  return false;
}
