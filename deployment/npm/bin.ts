#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";
import { runInstall } from "./install_api.ts";

declare const __dirname: string;

const exePath = join(
  __dirname,
  platform() === "win32" ? "dprint.exe" : "dprint",
);

if (!existsSync(exePath)) {
  try {
    runDprintExe(runInstall());
  } catch (err) {
    if (err instanceof Error) {
      console.error(err.message);
    } else {
      console.error(err);
    }
    process.exit(1);
  }
} else {
  runDprintExe(exePath);
}

function runDprintExe(exePath: string): void {
  const result = spawnSync(exePath, process.argv.slice(2), {
    stdio: "inherit",
  });
  if (result.error) {
    if (!existsSync(exePath)) {
      throw new Error(
        "Could not find exe at path '"
          + exePath
          + "'. Maybe try installing dprint again.",
      );
    }
    throw result.error;
  }

  process.exit(result.status ?? 1);
}
