const root = new URL("./", import.meta.url);
const dist = new URL("dist/", root);

interface Config {
  imports: Record<string, string>;
}

const config: Config = JSON.parse(await Deno.readTextFile(new URL("deno.json", root)));
const externals = Object.keys(config.imports).map((specifier) => specifier.endsWith("/") ? `${specifier}*` : specifier);

await Deno.remove(dist, { recursive: true }).catch(() => undefined);
await Deno.mkdir(dist, { recursive: true });
await bundle(["src/main.tsx"], externals);
await bundle(["src/formatter.worker.ts"], []);

const monacoCss = new URL(config.imports["monaco-editor"]);
monacoCss.search = "?css";
const generated = [
  `<link rel="stylesheet" href="${monacoCss}" />`,
  `<link rel="stylesheet" href="./main.css" />`,
  `<script type="importmap">${JSON.stringify({ imports: config.imports })}</script>`,
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
    `--outdir=${new URL(dist).pathname}`,
    ...external.map((specifier) => `--external=${specifier}`),
    ...entries,
  ];
  const { success, code } = await new Deno.Command(Deno.execPath(), { args, cwd: root.pathname, stdout: "inherit", stderr: "inherit" }).output();
  if (!success) throw new Error(`deno bundle ${entries.join(" ")} exited with ${code}`);
}
