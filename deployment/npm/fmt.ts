#!/usr/bin/env node
// `fmt [args]` runs `dprint fmt [args]` with the dprint of the main package.
// Without a configuration (no `--config` and none in the directory or its
// ancestors) it formats with kjanat's.
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import process from "node:process";
import { mainPackage } from "./names.ts";

declare const __filename: string;

const defaultConfig = "https://raw.githubusercontent.com/kjanat/kjanat/refs/heads/master/configs/dprint.remote.json";

// the names dprint looks for (POSSIBLE_CONFIG_FILE_NAMES in kprint-discovery)
const configFileNames = ["dprint.json", "dprint.jsonc", ".dprint.json", ".dprint.jsonc", "dprint.toml", ".dprint.toml"];

function specifiesConfig(args: string[]): boolean {
  return args.some((arg) => arg === "-c" || arg === "--config" || arg.startsWith("--config="));
}

function hasConfigFile(dir: string): boolean {
  for (let current = dir;; current = dirname(current)) {
    if (configFileNames.some((name) => existsSync(join(current, name)))) {
      return true;
    }
    if (dirname(current) === current) {
      return false;
    }
  }
}

const args = process.argv.slice(2);
const configArgs = specifiesConfig(args) || hasConfigFile(process.cwd()) ? [] : ["--config", defaultConfig];
const dprintBin = createRequire(__filename).resolve(`${mainPackage}/bin.cjs`);
const result = spawnSync(process.execPath, [dprintBin, "fmt", ...configArgs, ...args], { stdio: "inherit" });
if (result.error != null) {
  console.error(result.error.message);
  process.exit(1);
}
process.exit(result.status ?? 1);
