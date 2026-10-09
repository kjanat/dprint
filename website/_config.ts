import lume from "lume/mod.ts";
import codeHighlight from "lume/plugins/code_highlight.ts";
import date from "lume/plugins/date.ts";
import esbuild from "lume/plugins/esbuild.ts";
import nunjucks from "lume/plugins/nunjucks.ts";
import sass from "lume/plugins/sass.ts";
import anchor from "markdown-it-anchor";

await copyConfigSchema();

const site = lume(
  {
    src: "./src",
    location: new URL("https://dprint.kjanat.dev"),
  },
  {
    markdown: {
      options: {
        linkify: true,
      },
      plugins: [
        (md) =>
          anchor(md, {
            level: 2,
            permalink: anchor.permalink.headerLink(),
          }),
      ],
    },
  },
);

site
  .use(nunjucks())
  .use(sass())
  .use(date())
  .use(codeHighlight())
  .use(
    esbuild({
      options: {
        bundle: true,
        format: "iife",
        target: "es2024",
        minify: true,
        entryPoints: ["scripts.ts"],
      },
    }),
  )
  .add("scripts.ts")
  .add("style.scss")
  .add("theme.css")
  .copy("assets", ".");

// cache busting: give the built CSS/JS content-hashed filenames (like Vite)
// so each deploy invalidates stale browser caches, then rewrite the references
// in the generated HTML to point at the hashed names.
const hashedAssets = new Map<string, string>();

site.process([".css", ".js"], async (pages) => {
  for (const page of pages) {
    const url = page.data.url;
    const content = page.content;
    if (content === undefined) {
      throw new Error(`Cannot hash asset without content: ${url}`);
    }
    const dot = url.lastIndexOf(".");
    const hashedUrl = `${url.slice(0, dot)}.${await shortHash(content)}${url.slice(dot)}`;
    hashedAssets.set(url, hashedUrl);
    page.data.url = hashedUrl;
  }
});

site.process([".html"], (pages) => {
  for (const page of pages) {
    let html = page.content as string;
    for (const [from, to] of hashedAssets) {
      html = html.replaceAll(`"${from}"`, `"${to}"`);
    }
    page.content = html;
  }
});

export default site;

async function shortHash(content: string | Uint8Array): Promise<string> {
  const bytes = typeof content === "string"
    ? new TextEncoder().encode(content)
    : new Uint8Array(content);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("")
    .slice(0, 10);
}

async function copyConfigSchema() {
  // the config schema is generated from the configuration types in the dprint-config crate,
  // which keeps the published copy up to date (see its `the_published_schema_is_the_generated_one` test).
  // Pull it in here so it's served at https://dprint.kjanat.dev/schemas/v0.json.
  // This copy is gitignored.
  const source = new URL(
    "../crates/dprint-config/schema/v0.json",
    import.meta.url,
  );
  const destDir = new URL("./src/assets/schemas/", import.meta.url);
  await Deno.mkdir(destDir, { recursive: true });
  await Deno.copyFile(source, new URL("v0.json", destDir));
}
