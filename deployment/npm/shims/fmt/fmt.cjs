#!/usr/bin/env node
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  1 ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));

// deployment/npm/fmt.ts
var import_node_child_process = require("node:child_process");
var import_node_fs = require("node:fs");
var import_node_module = require("node:module");
var import_node_path = require("node:path");
var import_node_process = __toESM(require("node:process"));

// deployment/npm/names.ts
var scope = "@kprint";
var mainPackage = `${scope}/kprint`;

// deployment/npm/fmt.ts
var defaultConfig = "https://raw.githubusercontent.com/kjanat/kjanat/refs/heads/master/configs/dprint.remote.json";
var configFileNames = [
  "dprint.json",
  "dprint.jsonc",
  ".dprint.json",
  ".dprint.jsonc",
  "dprint.toml",
  ".dprint.toml"
];
function specifiesConfig(args2) {
  return args2.some((arg) => arg === "-c" || arg === "--config" || arg.startsWith("--config="));
}
function hasConfigFile(dir) {
  for (let current = dir; ; current = (0, import_node_path.dirname)(current)) {
    if (configFileNames.some((name) => (0, import_node_fs.existsSync)((0, import_node_path.join)(current, name)))) {
      return true;
    }
    if ((0, import_node_path.dirname)(current) === current) {
      return false;
    }
  }
}
var args = import_node_process.default.argv.slice(2);
var configArgs = specifiesConfig(args) || hasConfigFile(import_node_process.default.cwd()) ? [] : [
  "--config",
  defaultConfig
];
var dprintBin = (0, import_node_module.createRequire)(__filename).resolve(`${mainPackage}/bin.cjs`);
var result = (0, import_node_child_process.spawnSync)(import_node_process.default.execPath, [
  dprintBin,
  "fmt",
  ...configArgs,
  ...args
], {
  stdio: "inherit"
});
if (result.error != null) {
  console.error(result.error.message);
  import_node_process.default.exit(1);
}
import_node_process.default.exit(result.status ?? 1);
