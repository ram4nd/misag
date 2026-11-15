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

let context: AudioContext | null = null;

/** The audio context, made on first use and again if the old one closed (such as when its device went away). */
function audio(): AudioContext {
  if (!context || context.state === "closed") context = new AudioContext();
  return context;
}

/**
 * Makes the audio context and resumes it. Called on a key or click: a context
 * started without one may stay suspended, so a sound played later from a timer
 * would not be heard.
 */
export function primeAudio() {
  try {
    void audio().resume().catch(() => {});
  } catch {
    // playSound reports it, when a sound is due.
  }
}

/** Plays the sound; where Web Audio is missing or fails, calls `onFail` with why and plays nothing. */
export function playSound(name: Sound, onFail: (err: unknown) => void) {
  try {
    const ctx = audio();
    // Tones are scheduled on its clock, so they play once it runs.
    ctx.resume().catch(onFail);
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
  } catch (err) {
    onFail(err);
  }
}
