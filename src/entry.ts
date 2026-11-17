// The page's entry point: main.ts only defines the app, so tests can import it without starting it.
import { boot } from "./main";

try {
  boot();
} catch (err) {
  console.error("misag: could not start", err);
  document.body.textContent = `misag could not start (${err instanceof Error ? err.message : String(err)}). Restart the app.`;
  throw err;
}
