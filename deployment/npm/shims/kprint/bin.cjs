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

// deployment/npm/bin.ts
var import_node_child_process2 = require("node:child_process");
var import_node_fs = require("node:fs");
var import_node_os = require("node:os");
var import_node_path = require("node:path");

// deployment/npm/install_api.ts
var import_node_child_process = require("node:child_process");
var import_node_crypto = require("node:crypto");
var fs = __toESM(require("node:fs"));
var import_node_module = require("node:module");
var os = __toESM(require("node:os"));
var path = __toESM(require("node:path"));
var import_node_zlib = require("node:zlib");

// deployment/npm/names.ts
var scope = "@kprint";
var mainPackage = `${scope}/kprint`;
var platformPrefix = "kprint";
function platformPackage(platform3) {
  return `${scope}/${platformDir(platform3)}`;
}
function platformDir(platform3) {
  return `${platformPrefix}-${platform3}`;
}

// deployment/npm/install_api.ts
var cachedIsMusl;
function runInstall() {
  const dprintFileName = os.platform() === "win32" ? "dprint.exe" : "dprint";
  const targetExecutablePath = path.join(__dirname, dprintFileName);
  if (fs.existsSync(targetExecutablePath)) return targetExecutablePath;
  const target = getTarget();
  const sourceExecutablePath = resolveSourceExecutablePath(target, dprintFileName);
  if (sourceExecutablePath == null) {
    downloadExecutable(target, dprintFileName, targetExecutablePath);
    if (os.platform() !== "win32") chmodX(targetExecutablePath);
    return targetExecutablePath;
  }
  try {
    if (process.env.DPRINT_SIMULATED_READONLY_FILE_SYSTEM === "1") {
      console.warn("Simulating readonly file system for testing.");
      throw new Error("Throwing for testing purposes.");
    }
    hardLinkOrCopy(sourceExecutablePath, targetExecutablePath);
    if (os.platform() !== "win32") chmodX(targetExecutablePath);
    return targetExecutablePath;
  } catch (err) {
    if (process.env.DPRINT_DEBUG === "1") {
      console.warn(`Failed to copy executable from ${sourceExecutablePath} to ${targetExecutablePath}. Using resolved package path instead.`, err);
    }
    return sourceExecutablePath;
  }
}
function packageName(target) {
  return platformPackage(target);
}
function resolveSourceExecutablePath(target, dprintFileName) {
  let sourcePackagePath;
  try {
    sourcePackagePath = path.dirname((0, import_node_module.createRequire)(__filename).resolve(`${packageName(target)}/package.json`));
  } catch {
    return void 0;
  }
  const sourceExecutablePath = path.join(sourcePackagePath, dprintFileName);
  return fs.existsSync(sourceExecutablePath) ? sourceExecutablePath : void 0;
}
function readPackageField(fileName, field) {
  const parsed = JSON.parse(fs.readFileSync(path.join(__dirname, fileName), "utf8"));
  if (typeof parsed !== "object" || parsed === null) throw new Error(`${fileName} is not an object`);
  return Object.fromEntries(Object.entries(parsed))[field];
}
function downloadExecutable(target, dprintFileName, destinationPath) {
  const version = readPackageField("package.json", "version");
  if (typeof version !== "string") throw new Error("package.json has no version");
  const registry = (process.env.npm_config_registry || "https://registry.npmjs.org").replace(/\/+$/, "");
  const tarballUrl = `${registry}/${packageName(target)}/-/${platformDir(target)}-${version}.tgz`;
  console.error(`[kprint] Optional dependency ${packageName(target)} was not installed. Downloading from ${tarballUrl}`);
  const tarballBuffer = downloadBufferSync(tarballUrl);
  const executableBuffer = extractFileFromTarGzip(tarballBuffer, `package/${dprintFileName}`);
  verifyExecutableHash(target, executableBuffer);
  atomicWriteFile(destinationPath, executableBuffer);
}
function verifyExecutableHash(target, buffer) {
  let expected;
  try {
    expected = readPackageField("hashes.json", target);
  } catch (err) {
    throw new Error(`Could not load hashes.json to verify the downloaded binary: ${err instanceof Error ? err.message : err}`);
  }
  if (typeof expected !== "string") {
    throw new Error(`No known hash for ${packageName(target)} to verify the download against.`);
  }
  const actual = (0, import_node_crypto.createHash)("sha256").update(buffer).digest("hex");
  if (actual !== expected) {
    throw new Error(`Integrity check failed for the downloaded ${packageName(target)} binary.
  Expected sha256: ${expected}
  Actual sha256:   ${actual}`);
  }
}
function downloadBufferSync(url) {
  const fromCurl = downloadBufferWithCurl(url);
  return fromCurl != null ? fromCurl : downloadBufferWithNode(url);
}
function downloadBufferWithCurl(url) {
  const args = [
    "-fsSL"
  ];
  const proxy = process.env.npm_config_https_proxy || process.env.npm_config_proxy || process.env.HTTPS_PROXY || process.env.https_proxy;
  if (proxy) {
    args.push("--proxy", proxy);
  }
  const noProxy = process.env.npm_config_noproxy || process.env.NO_PROXY || process.env.no_proxy;
  if (noProxy) {
    args.push("--noproxy", noProxy);
  }
  args.push("-o", "-", url);
  try {
    return (0, import_node_child_process.execFileSync)("curl", args, {
      maxBuffer: 512 * 1024 * 1024
    });
  } catch (err) {
    if (err instanceof Error && "code" in err && err.code === "ENOENT") {
      return void 0;
    }
    throw err;
  }
}
function downloadBufferWithNode(url) {
  const script = `const https = require("node:https");
function f(u) {
  https.get(u, r => {
    const s = r.statusCode;
    if ((s === 301 || s === 302 || s === 303 || s === 307 || s === 308) && r.headers.location) {
      f(new URL(r.headers.location, u).toString());
      return;
    }
    if (s !== 200) {
      console.error(\`[dprint] Unexpected status code \${s} downloading \${u}\`);
      process.exit(1);
    }
    r.pipe(process.stdout);
  }).on("error", e => {
    console.error(String(e?.message || e));
    process.exit(1);
  });
}
f(process.argv[1]);
`;
  return (0, import_node_child_process.execFileSync)(process.execPath, [
    "-e",
    script,
    url
  ], {
    maxBuffer: 512 * 1024 * 1024
  });
}
function extractFileFromTarGzip(buffer, subpath) {
  let tar;
  try {
    tar = (0, import_node_zlib.gunzipSync)(buffer);
  } catch (err) {
    throw new Error(`Invalid gzip data in downloaded tarball: ${err instanceof Error ? err.message : err}`);
  }
  let offset = 0;
  while (offset < tar.length) {
    const name = readTarString(tar, offset, 100);
    const size = parseInt(readTarString(tar, offset + 124, 12).trim(), 8);
    offset += 512;
    if (!Number.isNaN(size)) {
      if (name === subpath) return tar.subarray(offset, offset + size);
      offset += size + 511 & ~511;
    }
  }
  throw new Error(`Could not find ${JSON.stringify(subpath)} in downloaded tarball`);
}
function readTarString(buffer, offset, length) {
  return buffer.toString("utf8", offset, offset + length).replace(/\0.*$/, "");
}
function atomicWriteFile(destinationPath, buffer) {
  const rand = (0, import_node_crypto.randomBytes)(4).toString("hex");
  const tempFilePath = `${destinationPath}.${rand}`;
  fs.writeFileSync(tempFilePath, buffer);
  try {
    fs.renameSync(tempFilePath, destinationPath);
  } catch (err) {
    try {
      fs.unlinkSync(tempFilePath);
    } catch {
    }
    throw err;
  }
}
function chmodX(filePath) {
  const fd = fs.openSync(filePath, "r");
  try {
    const perms = fs.fstatSync(fd).mode;
    fs.fchmodSync(fd, perms | 73);
  } finally {
    fs.closeSync(fd);
  }
}
function getTarget() {
  const platform3 = os.platform();
  if (platform3 === "linux") {
    return `${platform3}-${getArch()}-${getLinuxFamily()}`;
  } else {
    return `${platform3}-${getArch()}`;
  }
}
function getArch() {
  const arch2 = os.arch();
  if (arch2 !== "arm64" && arch2 !== "x64" && arch2 !== "riscv64" && arch2 !== "loong64" && arch2 !== "ppc64") {
    throw new Error(`Unsupported architecture ${os.arch()}. Only x64, arm64, riscv64, loong64 and ppc64 binaries are available.`);
  }
  return arch2;
}
function getLinuxFamily() {
  return getIsMusl() ? "musl" : "glibc";
  function getIsMusl() {
    if (cachedIsMusl == null) {
      cachedIsMusl = innerGet();
    }
    return cachedIsMusl;
    function innerGet() {
      try {
        if (os.platform() !== "linux") {
          return false;
        }
        let musl = isMuslFromFilesystem();
        if (musl == null) {
          musl = isMuslFromReport();
        }
        if (musl == null) {
          musl = isMuslFromChildProcess();
        }
        return musl;
      } catch (err) {
        console.warn("Error checking if musl.", err);
        return false;
      }
    }
    function isMuslFromFilesystem() {
      try {
        return fs.readFileSync("/usr/bin/ldd", "utf-8").includes("musl");
      } catch {
        return null;
      }
    }
    function isMuslFromReport() {
      const processReport = process.report;
      if (typeof processReport !== "object" || processReport === null || !("getReport" in processReport) || typeof processReport.getReport !== "function") {
        return null;
      }
      const originalExcludeNetwork = Reflect.get(processReport, "excludeNetwork");
      let rawReport;
      try {
        Reflect.set(processReport, "excludeNetwork", true);
        rawReport = processReport.getReport();
      } finally {
        Reflect.set(processReport, "excludeNetwork", originalExcludeNetwork);
      }
      const report = typeof rawReport === "string" ? JSON.parse(rawReport) : rawReport;
      if (typeof report !== "object" || report === null) {
        return null;
      }
      const fields = Object.fromEntries(Object.entries(report));
      const header = fields.header;
      if (typeof header === "object" && header !== null && "glibcVersionRuntime" in header && header.glibcVersionRuntime) {
        return false;
      }
      const sharedObjects = fields.sharedObjects;
      if (Array.isArray(sharedObjects)) {
        return sharedObjects.some((entry) => typeof entry === "string" && isFileMusl(entry));
      }
      return false;
    }
    function isMuslFromChildProcess() {
      try {
        return (0, import_node_child_process.execSync)("ldd --version", {
          encoding: "utf8"
        }).includes("musl");
      } catch {
        return false;
      }
    }
    function isFileMusl(f) {
      return f.includes("libc.musl-") || f.includes("ld-musl-");
    }
  }
}
function hardLinkOrCopy(sourcePath, destinationPath) {
  try {
    fs.linkSync(sourcePath, destinationPath);
  } catch {
    atomicCopyFile(sourcePath, destinationPath);
  }
}
function atomicCopyFile(sourcePath, destinationPath) {
  const rand = (0, import_node_crypto.randomBytes)(4).toString("hex");
  const tempFilePath = `${destinationPath}.${rand}`;
  fs.copyFileSync(sourcePath, tempFilePath);
  try {
    fs.renameSync(tempFilePath, destinationPath);
  } catch (err) {
    try {
      fs.unlinkSync(tempFilePath);
    } catch {
    }
    throw err;
  }
}

// deployment/npm/bin.ts
var exePath = (0, import_node_path.join)(__dirname, (0, import_node_os.platform)() === "win32" ? "dprint.exe" : "dprint");
if (!(0, import_node_fs.existsSync)(exePath)) {
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
function runDprintExe(exePath2) {
  const result = (0, import_node_child_process2.spawnSync)(exePath2, process.argv.slice(2), {
    stdio: "inherit"
  });
  if (result.error) {
    if (!(0, import_node_fs.existsSync)(exePath2)) {
      throw new Error("Could not find exe at path '" + exePath2 + "'. Maybe try installing dprint again.");
    }
    throw result.error;
  }
  process.exit(result.status ?? 1);
}
