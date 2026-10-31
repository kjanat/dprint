import { fileURLToPath } from "node:url";

const root = new URL("./", import.meta.url);
const dist = new URL("dist/", root);

interface Config {
  imports: Record<string, string>;
}

const config: Config = JSON.parse(await Deno.readTextFile(new URL("deno.json", root)));
// npm: packages get bundled (their lock hash is stable); URL imports stay external and load via the import map
const browserImports = Object.fromEntries(Object.entries(config.imports).filter(([, target]) => !target.startsWith("npm:")));
const externals = Object.keys(browserImports).map((specifier) => specifier.endsWith("/") ? `${specifier}*` : specifier);

await Deno.remove(dist, { recursive: true }).catch(() => undefined);
await Deno.mkdir(dist, { recursive: true });
await bundle(["src/main.tsx"], externals);
await bundle(["src/formatter.worker.ts"], []);

const monacoCss = new URL(config.imports["monaco-editor"]);
monacoCss.search = "?css";
const generated = [
  `<link rel="stylesheet" href="${monacoCss}" />`,
  `<link rel="stylesheet" href="./main.css" />`,
  `<script type="importmap">${JSON.stringify({ imports: browserImports })}</script>`,
].join("\n    ");
const html = await Deno.readTextFile(new URL("index.html", root));
if (!html.includes("<!-- generated -->")) throw new Error("index.html has no <!-- generated --> placeholder");
await Deno.writeTextFile(new URL("index.html", dist), html.replace("<!-- generated -->", generated));

async function bundle(entries: string[], external: string[]) {
  const args = [
    "bundle",
    "--platform=browser",
    "--format=esm",
    "--minify",
    `--outdir=${fileURLToPath(dist)}`,
    ...external.map((specifier) => `--external=${specifier}`),
    ...entries,
  ];
  const { success, code } = await new Deno.Command(Deno.execPath(), { args, cwd: fileURLToPath(root), stdout: "inherit", stderr: "inherit" }).output();
  if (!success) throw new Error(`deno bundle ${entries.join(" ")} exited with ${code}`);
}
