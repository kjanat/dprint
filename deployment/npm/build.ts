#!/usr/bin/env -S deno run -A
import $ from "@david/dax";
import { parseArgs } from "@std/cli/parse-args";
// @ts-types="@types/decompress"
import decompress from "decompress";

interface Package {
  zipFileName: string;
  os: "win32" | "darwin" | "linux" | "android" | "freebsd";
  cpu: "x64" | "arm64" | "riscv64" | "loong64" | "ppc64";
  libc?: "glibc" | "musl";
}

const args = parseArgs(Deno.args, {
  boolean: ["publish", "publish-only"],
});
const packages: Package[] = [
  {
    zipFileName: "dprint-x86_64-unknown-freebsd.zip",
    os: "freebsd",
    cpu: "x64",
  },
  {
    zipFileName: "dprint-x86_64-pc-windows-msvc.zip",
    os: "win32",
    cpu: "x64",
  },
  {
    zipFileName: "dprint-aarch64-pc-windows-msvc.zip",
    os: "win32",
    cpu: "arm64",
  },
  {
    zipFileName: "dprint-x86_64-apple-darwin.zip",
    os: "darwin",
    cpu: "x64",
  },
  {
    zipFileName: "dprint-aarch64-apple-darwin.zip",
    os: "darwin",
    cpu: "arm64",
  },
  {
    zipFileName: "dprint-x86_64-unknown-linux-gnu.zip",
    os: "linux",
    cpu: "x64",
    libc: "glibc",
  },
  {
    zipFileName: "dprint-x86_64-unknown-linux-musl.zip",
    os: "linux",
    cpu: "x64",
    libc: "musl",
  },
  {
    zipFileName: "dprint-aarch64-unknown-linux-gnu.zip",
    os: "linux",
    cpu: "arm64",
    libc: "glibc",
  },
  {
    zipFileName: "dprint-aarch64-unknown-linux-musl.zip",
    os: "linux",
    cpu: "arm64",
    libc: "musl",
  },
  {
    zipFileName: "dprint-riscv64gc-unknown-linux-gnu.zip",
    os: "linux",
    cpu: "riscv64",
    libc: "glibc",
  },
  {
    zipFileName: "dprint-loongarch64-unknown-linux-gnu.zip",
    os: "linux",
    cpu: "loong64",
    libc: "glibc",
  },
  {
    zipFileName: "dprint-loongarch64-unknown-linux-musl.zip",
    os: "linux",
    cpu: "loong64",
    libc: "musl",
  },
  {
    // Node reports ppc64le as "ppc64"
    zipFileName: "dprint-powerpc64le-unknown-linux-gnu.zip",
    os: "linux",
    cpu: "ppc64",
    libc: "glibc",
  },
  {
    zipFileName: "dprint-powerpc64le-unknown-linux-musl.zip",
    os: "linux",
    cpu: "ppc64",
    libc: "musl",
  },
  {
    // android (Termux): Node reports the platform as "android" and the arch as
    // "arm64"/"x64". bionic libc, so no libc field (npm only knows glibc/musl).
    zipFileName: "dprint-aarch64-linux-android.zip",
    os: "android",
    cpu: "arm64",
  },
  {
    zipFileName: "dprint-x86_64-linux-android.zip",
    os: "android",
    cpu: "x64",
  },
];

const markdownText = `# kprint

npm CLI distribution for [dprint](https://dprint.kjanat.dev)—a pluggable and configurable code formatting platform.
`;

const currentDir = $.path(import.meta.url).parentOrThrow();
const rootDir = currentDir.parentOrThrow().parentOrThrow();
const outputDir = currentDir.join("./dist");
const scopeDir = outputDir.join("@kprint");
const dprintDir = outputDir.join("kprint");
const version = resolveVersion();

$.logStep(`Publishing ${version}...`);

if (!args["publish-only"]) {
  // Resolve the permanent repository ID so release URLs and metadata survive renames.
  const repository = await $.request(
    "https://api.github.com/repositories/1092062077",
  ).json<{ html_url: string }>();
  const repositoryUrl = repository.html_url;
  await $`rm -rf ${outputDir}`;
  await $`mkdir -p ${dprintDir} ${scopeDir}`;

  // setup dprint packages
  {
    $.logStep(`Setting up kprint ${version}...`);
    const pkgJson = {
      name: "kprint",
      version: version,
      description: "Pluggable and configurable code formatting platform written in Rust.",
      bin: { kprint: "bin.cjs", dprint: "bin.cjs" },
      repository: {
        type: "git",
        url: `git+${repositoryUrl}.git`,
      },
      keywords: ["code", "formatter"],
      author: "Kaj Kowalski",
      license: "MIT",
      bugs: {
        url: `${repositoryUrl}/issues`,
      },
      homepage: `${repositoryUrl}#readme`,
      // for yarn berry (https://github.com/dprint/dprint/issues/686)
      preferUnplugged: true,
      scripts: {
        postinstall: "node ./install.cjs",
      },
      optionalDependencies: Object.fromEntries(
        packages.map((pkg) => [`@kprint/${getPackageNameNoScope(pkg)}`, version]),
      ),
    };
    for (const entry of ["bin", "install"]) {
      await $`deno bundle --platform=deno --format=cjs --output=${dprintDir.join(`${entry}.cjs`)} ${currentDir.join(`${entry}.ts`)}`;
    }
    dprintDir.join("package.json").writeJsonPrettySync(pkgJson);
    rootDir.join("LICENSE").copyFileSync(dprintDir.join("LICENSE"));
    dprintDir.join("README.md").writeTextSync(markdownText);
    // ensure the test files don't get published
    dprintDir.join(".npmignore").writeTextSync("dprint\ndprint.exe\n");

    // setup each binary package
    const executableHashes: Record<string, string> = {};
    for (const pkg of packages) {
      const pkgName = getPackageNameNoScope(pkg);
      $.logStep(`Setting up @kprint/${pkgName}...`);
      const pkgDir = scopeDir.join(pkgName);
      const zipPath = pkgDir.join("output.zip");

      await $`mkdir -p ${pkgDir}`;

      // download and extract the zip file
      const zipUrl = `${repositoryUrl}/releases/download/${version}/${pkg.zipFileName}`;
      await $.request(zipUrl).showProgress().pipeToPath(zipPath);
      await decompress(zipPath.toString(), pkgDir.toString());
      zipPath.removeSync();

      // record the executable's hash so the dprint package can verify it when
      // downloading the binary as a fallback (e.g. for `npm install --omit=optional`)
      const executableName = pkg.os === "win32" ? "dprint.exe" : "dprint";
      executableHashes[pkgName] = await sha256Hex(
        pkgDir.join(executableName).toString(),
      );

      // create the package.json and readme
      pkgDir
        .join("README.md")
        .writeTextSync(
          `# @kprint/${pkgName}\n\n${pkgName} distribution of dprint.\n`,
        );
      pkgDir.join("package.json").writeJsonPrettySync({
        name: `@kprint/${pkgName}`,
        version: version,
        description: `${pkgName} distribution of the dprint code formatter`,
        repository: {
          type: "git",
          url: `git+${repositoryUrl}.git`,
        },
        // force yarn to unpack
        preferUnplugged: true,
        author: "Kaj Kowalski",
        license: "MIT",
        bugs: {
          url: `${repositoryUrl}/issues`,
        },
        homepage: `${repositoryUrl}#readme`,
        os: [pkg.os],
        cpu: [pkg.cpu],
        libc: pkg.libc == null ? undefined : [pkg.libc],
      });
    }

    // write the executable hashes into the dprint package so the download
    // fallback can verify the binary it fetches from the registry
    dprintDir.join("hashes.json").writeJsonPrettySync(executableHashes);
  }

  // verify that the package is created correctly
  {
    $.logStep("Verifying packages...");
    const testPlatform = Deno.build.os === "windows"
      ? Deno.build.arch === "x86_64"
        ? "@kprint/win32-x64"
        : "@kprint/win32-arm64"
      : Deno.build.os === "darwin"
      ? Deno.build.arch === "x86_64"
        ? "@kprint/darwin-x64"
        : "@kprint/darwin-arm64"
      : Deno.build.arch === "x86_64"
      ? "@kprint/linux-x64-glibc"
      : "@kprint/linux-arm64-glibc";
    $.logLight("Test platform:", testPlatform);
    outputDir.join("package.json").writeJsonPrettySync({
      workspaces: [
        "kprint",
        // There seems to be a bug with npm workspaces where this doesn't
        // work, so for now make some assumptions and only include the package
        // that works on the CI for the current operating system
        // ...packages.map(p => `@kprint/${getPackageNameNoScope(p)}`),
        testPlatform,
      ],
    });

    const dprintExe = Deno.build.os === "windows" ? "dprint.exe" : "dprint";
    await $`npm install`.cwd(dprintDir);

    // ensure the post-install script adds the executable to the dprint package,
    // which is necessary for faster caching and to ensure the vscode extension
    // picks it up
    if (!dprintDir.join(dprintExe).existsSync()) {
      throw new Error("dprint executable did not exist after post install");
    }

    // run once after post install created dprint, once with a simulated readonly file system, once creating the cache and once with
    await $`node bin.cjs -v && rm ${dprintExe} && DPRINT_SIMULATED_READONLY_FILE_SYSTEM=1 node bin.cjs -v && node bin.cjs -v && node bin.cjs -v`.cwd(
      dprintDir,
    );

    if (!dprintDir.join(dprintExe).existsSync()) {
      throw new Error(
        "dprint executable did not exist when lazily initialized",
      );
    }
  }
}

// publish if necessary
if (args.publish || args["publish-only"]) {
  for (const pkg of packages) {
    const pkgName = getPackageNameNoScope(pkg);
    $.logStep(`Publishing @kprint/${pkgName}...`);
    if (await checkPackagePublished(`@kprint/${pkgName}`)) {
      $.logLight("  Already published.");
      continue;
    }
    const pkgDir = scopeDir.join(pkgName);
    // ensure the binary is executable in the tarball
    if (pkg.os !== "win32") {
      await $`chmod +x ${pkgDir.join("dprint")}`;
    }
    await $`cd ${pkgDir} && npm publish --provenance --access public`;
  }

  $.logStep(`Publishing kprint...`);
  await $`cd ${dprintDir} && npm publish --provenance --access public`;
}

async function sha256Hex(filePath: string) {
  const data = await Deno.readFile(filePath);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function getPackageNameNoScope(name: Package) {
  const libc = name.libc == null ? "" : `-${name.libc}`;
  return `${name.os}-${name.cpu}${libc}`;
}

function resolveVersion() {
  const firstArg = args._[0];
  if (
    firstArg != null
    && typeof firstArg === "string"
    && firstArg.trim().length > 0
  ) {
    return firstArg;
  }
  const version = rootDir
    .join("crates/kprint/Cargo.toml")
    .readTextSync()
    .match(/version = "(.*?)"/)?.[1];
  if (version == null) {
    throw new Error("Could not resolve version.");
  }
  return version;
}

async function checkPackagePublished(pkgName: string) {
  const result = await $`npm info ${pkgName}@${version}`.quiet().noThrow();
  return result.code === 0;
}
