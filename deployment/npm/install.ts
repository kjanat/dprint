import { replaceBinEntry, runInstall } from "./install_api.ts";

const exePath = runInstall();
try {
  replaceBinEntry(exePath);
} catch {
  // ignore - falls back to bin.cjs
}
