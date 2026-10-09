#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { AsyncFunctionArguments } from "@actions/github-script";

type Actions = Pick<AsyncFunctionArguments, "core" | "github" | "context">;
type Log = (message: string) => void;
type Name = "official" | "master" | "pr";
type Build = "master" | "pr";

interface Measurement {
  time_wall_clock: { value: number };
  memory_peak_resident: { value: number };
}

interface Result {
  name: string;
  measurements: Measurement[];
}

interface Meta {
  repository: string;
  versions: Record<Name, string>;
  shas: Record<Build, string>;
  discovery: Record<Build, string>;
  files: number;
  git: string;
  plugins: string[];
}

interface Welch {
  diff: number;
  low: number;
  high: number;
  p: number;
  n: [number, number];
}

interface WorkflowRun {
  head_sha: string;
  head_branch: string;
  head_repository: { owner: { login: string } };
}

const USAGE = `Usage:
  bench.ts setup <builds>
  bench.ts bench <corpus> <results> <builds>
  bench.ts report <results> <comment.md>
  bench.ts comment <comment.md>`;

const NAMES: Name[] = ["official", "master", "pr"];
const MARKER = "<!-- dprint-bench -->";
const LABELS: Record<Name, string> = { official: "official", master: "master", pr: "this PR" };
const OFFICIAL = "https://github.com/dprint/dprint/releases/latest/download/dprint-x86_64-unknown-linux-gnu.zip";
const HYPERFINE = {
  url: "https://github.com/sharkdp/hyperfine/releases/download/v2.0.0/hyperfine-v2.0.0-x86_64-unknown-linux-gnu.tar.gz",
  sha256: "ae2beda2ac99c098427e4f755244552daf20e514422889cca6d44421e2d93c87",
  member: "hyperfine-v2.0.0-x86_64-unknown-linux-gnu/hyperfine",
};
const PLUGINS: [string, string][] = [
  ["typescript-0.96.1", "9c52244de2c25a33addc7ddacff4443234a2dc4b5d3f34131262d1e0df35007c"],
  ["json-0.25.2", "811ca4c6f732bcd6f707bc2f42d42395bb4127cdf829960cf628a18fc149777e"],
  ["markdown-0.26.0", "5cebd0459bf1aa13fb027331720158d03e2a897b1d9f7b3bf93cac661ca4ddbd"],
  ["toml-0.9.0", "776e088ed4255a51a80e3640e5fd20b15661147e86df3e99611aaa37e9c59852"],
  ["dockerfile-0.7.0", "04e0bef5fb60355247da7937af0df9641bf93922c60aaa19cc94ca141e293d1d"],
];
const SCENARIOS = [
  { name: "check-cold", label: "`check`, empty cache", subcommand: "check", cold: true, rounds: 3, hyperfine: ["--runs", "2"] },
  { name: "check-warm", label: "`check`, warm cache", subcommand: "check", cold: false, rounds: 6, hyperfine: ["--warmup", "1", "--runs", "5"] },
  { name: "fmt-warm", label: "`fmt`, warm cache", subcommand: "fmt", cold: false, rounds: 6, hyperfine: ["--warmup", "1", "--runs", "5"] },
];

function run(command: string, args: string[], cwd?: string): void {
  const result = spawnSync(command, args, { cwd, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} exited with ${result.status}`);
}

function capture(command: string, args: string[], cwd?: string) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], maxBuffer: 64 << 20 });
  if (result.error) throw result.error;
  return result;
}

function quote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

async function download(url: string, file: string, sha256?: string): Promise<void> {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (sha256) {
    const actual = createHash("sha256").update(bytes).digest("hex");
    if (actual !== sha256) throw new Error(`${url}: sha256 ${actual}, expected ${sha256}`);
  }
  await writeFile(file, bytes);
}

async function setup(builds: string): Promise<void> {
  const official = join(builds, "dprint-official");
  await mkdir(official, { recursive: true });
  const zip = join(builds, "dprint-official.zip");
  await download(OFFICIAL, zip);
  run("unzip", ["-q", "-o", zip, "-d", official]);
  await rm(zip);
  const tarball = join(builds, "hyperfine.tar.gz");
  await download(HYPERFINE.url, tarball, HYPERFINE.sha256);
  run("tar", ["-xzf", tarball, "-C", builds, "--strip-components=1", HYPERFINE.member]);
  await rm(tarball);
}

async function bench(corpus: string, results: string, builds: string): Promise<void> {
  const binary = (name: Name) => join(builds, `dprint-${name}`, "dprint");
  const hyperfine = join(builds, "hyperfine");
  for (const file of [...NAMES.map(binary), hyperfine]) await chmod(file, 0o755);
  await mkdir(results, { recursive: true });
  const work = await mkdtemp(join(tmpdir(), "dprint-bench-"));
  try {
    await mkdir(join(work, "plugins"));
    const pluginPaths: string[] = [];
    for (const [plugin, sha256] of PLUGINS) {
      const file = join(work, "plugins", `${plugin}.wasm`);
      await download(`https://plugins.dprint.dev/${plugin}.wasm`, file, sha256);
      pluginPaths.push(file);
    }
    const config = join(corpus, "dprint.json");
    await writeFile(
      config,
      JSON.stringify(
        {
          lineWidth: 160,
          indentWidth: 2,
          excludes: ["**/node_modules", "**/target", "**/dist", "**/_site", "**/*-lock.json"],
          plugins: pluginPaths,
        },
        null,
        2,
      ) + "\n",
    );
    const command = (name: Name, subcommand: string) =>
      `env DPRINT_CACHE_DIR=${quote(join(work, `cache-${name}`))} ${quote(binary(name))} ${subcommand} --config ${quote(config)}`;

    run(binary("official"), ["fmt", "--config", config], corpus);
    run("git", ["config", "core.untrackedCache", "true"], corpus);
    run("git", ["config", "core.fsmonitor", "true"], corpus);
    run("git", ["status", "--porcelain"], corpus);
    run("git", ["status", "--porcelain"], corpus);

    for (const scenario of SCENARIOS) {
      for (let round = 0; round < scenario.rounds; round++) {
        const start = round % NAMES.length;
        const order = [...NAMES.slice(start), ...NAMES.slice(0, start)];
        const args = ["--export-json", join(results, `${scenario.name}.${round}.json`), ...scenario.hyperfine];
        for (const name of order) args.push("--command-name", name);
        for (const name of order) {
          if (scenario.cold) args.push("--prepare", command(name, "clear-cache"));
          args.push(command(name, scenario.subcommand));
        }
        run(hyperfine, args, corpus);
      }
    }

    const version = (name: Name) => capture(binary(name), ["--version"]).stdout.trim();
    const sha = async (name: Build) => (await readFile(join(builds, `dprint-${name}`, "sha"), "utf8").catch(() => "")).trim();
    const discovery = (name: Build) => {
      const { stdout, stderr } = capture(binary(name), ["check", "--config", config, "--log-level=debug"], corpus);
      return (stdout + stderr).match(/Read \d+ files from the git index|Not (?:using|reading) the git index.*/)?.[0] ?? "no git index message";
    };
    const files = capture(binary("official"), ["output-file-paths", "--config", config], corpus).stdout.split("\n").filter(Boolean).length;

    const meta: Meta = {
      repository: process.env.GITHUB_SERVER_URL && process.env.GITHUB_REPOSITORY ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}` : "",
      versions: { official: version("official"), master: version("master"), pr: version("pr") },
      shas: { master: await sha("master"), pr: await sha("pr") },
      discovery: { master: discovery("master"), pr: discovery("pr") },
      files,
      git: capture("git", ["--version"]).stdout.trim(),
      plugins: PLUGINS.map(([plugin]) => plugin),
    };
    await writeFile(join(results, "meta.json"), JSON.stringify(meta, null, 2) + "\n");
  } finally {
    spawnSync("git", ["fsmonitor--daemon", "stop"], { cwd: corpus, stdio: "ignore" });
    await rm(work, { recursive: true, force: true });
  }
}

function mean(samples: number[]): number {
  return samples.reduce((sum, sample) => sum + sample, 0) / samples.length;
}

function stdev(samples: number[]): number {
  const average = mean(samples);
  return Math.sqrt(samples.reduce((sum, sample) => sum + (sample - average) ** 2, 0) / (samples.length - 1));
}

function formatSeconds(value: number, spread: number): string {
  if (value >= 1) return `${value.toFixed(2)} s ± ${spread.toFixed(2)} s`;
  return `${(value * 1000).toFixed(1)} ms ± ${(spread * 1000).toFixed(1)} ms`;
}

function formatBytes(value: number, spread: number): string {
  return `${(value / (1 << 20)).toFixed(0)} MiB ± ${(spread / (1 << 20)).toFixed(0)} MiB`;
}

function logGamma(x: number): number {
  const coefficients = [76.18009172947146, -86.50532032941677, 24.01409824083091, -1.231739572450155, 0.1208650973866179e-2, -0.5395239384953e-5];
  let y = x;
  let temp = x + 5.5;
  temp -= (x + 0.5) * Math.log(temp);
  let series = 1.000000000190015;
  for (const coefficient of coefficients) series += coefficient / ++y;
  return -temp + Math.log((2.5066282746310005 * series) / x);
}

function betaContinuedFraction(a: number, b: number, x: number): number {
  const tiny = 1e-300;
  const qab = a + b;
  const qap = a + 1;
  const qam = a - 1;
  let c = 1;
  let d = 1 - (qab * x) / qap;
  if (Math.abs(d) < tiny) d = tiny;
  d = 1 / d;
  let h = d;
  for (let m = 1; m <= 200; m++) {
    const m2 = 2 * m;
    let coefficient = (m * (b - m) * x) / ((qam + m2) * (a + m2));
    d = 1 + coefficient * d;
    if (Math.abs(d) < tiny) d = tiny;
    c = 1 + coefficient / c;
    if (Math.abs(c) < tiny) c = tiny;
    d = 1 / d;
    h *= d * c;
    coefficient = (-(a + m) * (qab + m) * x) / ((a + m2) * (qap + m2));
    d = 1 + coefficient * d;
    if (Math.abs(d) < tiny) d = tiny;
    c = 1 + coefficient / c;
    if (Math.abs(c) < tiny) c = tiny;
    d = 1 / d;
    const delta = d * c;
    h *= delta;
    if (Math.abs(delta - 1) < 3e-14) break;
  }
  return h;
}

function regularizedBeta(x: number, a: number, b: number): number {
  if (x <= 0) return 0;
  if (x >= 1) return 1;
  const front = Math.exp(logGamma(a + b) - logGamma(a) - logGamma(b) + a * Math.log(x) + b * Math.log(1 - x));
  return x < (a + 1) / (a + b + 2) ? (front * betaContinuedFraction(a, b, x)) / a : 1 - (front * betaContinuedFraction(b, a, 1 - x)) / b;
}

function studentTwoSided(t: number, df: number): number {
  return regularizedBeta(df / (df + t * t), df / 2, 0.5);
}

function studentCritical(df: number, alpha: number): number {
  let low = 0;
  let high = 1000;
  for (let i = 0; i < 200; i++) {
    const mid = (low + high) / 2;
    if (studentTwoSided(mid, df) > alpha) low = mid;
    else high = mid;
  }
  return (low + high) / 2;
}

function welch(a: number[], b: number[]): Welch {
  const diff = mean(a) - mean(b);
  const varianceA = stdev(a) ** 2 / a.length;
  const varianceB = stdev(b) ** 2 / b.length;
  const se = Math.sqrt(varianceA + varianceB);
  if (se === 0) return { diff, low: diff, high: diff, p: diff === 0 ? 1 : 0, n: [a.length, b.length] };
  const df = (varianceA + varianceB) ** 2 / (varianceA ** 2 / (a.length - 1) + varianceB ** 2 / (b.length - 1));
  const margin = studentCritical(df, 0.05) * se;
  return { diff, low: diff - margin, high: diff + margin, p: studentTwoSided(Math.abs(diff) / se, df), n: [a.length, b.length] };
}

function percent(value: number): string {
  return `${value >= 0 ? "+" : ""}${Math.round(value * 100)}%`;
}

function pValue(p: number): string {
  return p < 0.001 ? "p < 0.001" : `p = ${p.toFixed(3)}`;
}

function compare(merged: Record<Name, Measurement[]>) {
  const samples = (name: Name) => merged[name].map((measurement) => measurement.time_wall_clock.value);
  const test = welch(samples("pr"), samples("master"));
  const base = mean(samples("master"));
  const verdict = test.p >= 0.05 ? "no significant difference" : test.diff < 0 ? "faster" : "slower";
  const order = [...NAMES].sort((x, y) => mean(samples(x)) - mean(samples(y)));
  let ranking = LABELS[order[0]];
  for (let i = 1; i < order.length; i++) {
    ranking += (welch(samples(order[i - 1]), samples(order[i])).p < 0.05 ? " < " : " ≈ ") + LABELS[order[i]];
  }
  return { verdict, change: test.diff / base, low: test.low / base, high: test.high / base, p: test.p, n: test.n, ranking };
}

function commitLink(repository: string, sha: string): string {
  if (!sha) return "";
  const short = `\`${sha.slice(0, 7)}\``;
  return repository ? ` [${short}](${repository}/commit/${sha})` : ` ${short}`;
}

function releaseLink(version: string): string {
  const tag = version.match(/^dprint (\S+)$/)?.[1];
  return tag ? `[\`${version}\`](https://github.com/dprint/dprint/releases/tag/${tag})` : `\`${version}\``;
}

async function measurements(results: string, scenario: string): Promise<Record<Name, Measurement[]>> {
  const merged: Record<Name, Measurement[]> = { official: [], master: [], pr: [] };
  const files = (await readdir(results)).filter((file) => file.startsWith(`${scenario}.`) && file.endsWith(".json")).sort();
  for (const file of files) {
    const exported: { results: Result[] } = JSON.parse(await readFile(join(results, file), "utf8"));
    for (const result of exported.results) {
      const name = NAMES.find((candidate) => candidate === result.name);
      if (!name) throw new Error(`${file}: unknown command name ${result.name}`);
      merged[name].push(...result.measurements);
    }
  }
  return merged;
}

function cells(merged: Record<Name, Measurement[]>, metric: keyof Measurement, format: (value: number, spread: number) => string): string[] {
  const stat = (name: Name) => {
    const samples = merged[name].map((measurement) => measurement[metric].value);
    return { value: mean(samples), spread: stdev(samples) };
  };
  const stats = { official: stat("official"), master: stat("master"), pr: stat("pr") };
  return NAMES.map((name) => {
    let text = format(stats[name].value, stats[name].spread);
    if (name !== "official") text += ` · ${(stats.official.value / stats[name].value).toFixed(1)}×`;
    if (name === "pr") {
      const change = Math.round(((stats.pr.value - stats.master.value) / stats.master.value) * 100);
      text += ` · ${change >= 0 ? "+" : ""}${change}% vs master`;
    }
    return text;
  });
}

async function render(results: string): Promise<string> {
  const meta: Meta = JSON.parse(await readFile(join(results, "meta.json"), "utf8"));
  const header = `| | ${releaseLink(meta.versions.official)} | master${commitLink(meta.repository, meta.shas.master)} | this PR${
    commitLink(meta.repository, meta.shas.pr)
  } |`;
  const loaded = [];
  for (const scenario of SCENARIOS) loaded.push({ label: scenario.label, merged: await measurements(results, scenario.name) });
  const compared = loaded.map(({ label, merged }) => ({ label, ...compare(merged) }));
  const bounds = compared.flatMap(({ low, high }) => [low, high, 0]).map((value) => value * 100);
  const lines = [
    MARKER,
    "## Benchmark",
    "",
    "**This PR vs master**, wall time. Welch's t-test, 95% confidence interval of the change relative to master's mean.",
    "",
    ...compared.map(({ label, verdict, change, low, high, p, n, ranking }) =>
      `- ${label}: **${verdict}**, ${percent(change)} (95% CI ${percent(low)} to ${percent(high)}, ${pValue(p)}, n = ${n[0]} vs ${n[1]}). ${ranking}.`
    ),
    "",
    "```mermaid",
    "xychart-beta",
    "    title \"Change vs master in %, bar is the mean, lines are the 95% CI\"",
    `    x-axis [${SCENARIOS.map((scenario) => `"${scenario.name}"`).join(", ")}]`,
    `    y-axis "%" ${Math.floor(Math.min(...bounds) / 10) * 10 - 10} --> ${Math.ceil(Math.max(...bounds) / 10) * 10 + 10}`,
    `    bar [${compared.map(({ change }) => Math.round(change * 100)).join(", ")}]`,
    `    line [${compared.map(({ low }) => Math.round(low * 100)).join(", ")}]`,
    `    line [${compared.map(({ high }) => Math.round(high * 100)).join(", ")}]`,
    "```",
    "",
    "Wall time, mean ± σ, with the speedup over the official release.",
    "",
    header,
    "| --- | ---: | ---: | ---: |",
    ...loaded.map(({ label, merged }) => `| ${label} | ${cells(merged, "time_wall_clock", formatSeconds).join(" | ")} |`),
    "",
    "Peak memory, mean ± σ, with the reduction against the official release.",
    "",
    header,
    "| --- | ---: | ---: | ---: |",
    ...loaded.map(({ label, merged }) => `| ${label} | ${cells(merged, "memory_peak_resident", formatBytes).join(" | ")} |`),
    "",
    `${meta.files} files of this pull request's checkout, formatted with ${meta.plugins.map((plugin) => `\`${plugin}\``).join(", ")}. ${meta.git}.`,
    "",
    `- master: ${meta.discovery.master}`,
    `- this PR: ${meta.discovery.pr}`,
  ];
  return lines.join("\n") + "\n";
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null) throw new Error(`${path} is not an object`);
  return Object.fromEntries(Object.entries(value));
}

function string(value: unknown, path: string): string {
  if (typeof value !== "string") throw new Error(`${path} is not a string`);
  return value;
}

function workflowRun(payload: unknown): WorkflowRun {
  const run = record(record(payload, "payload").workflow_run, "payload.workflow_run");
  const owner = record(record(run.head_repository, "workflow_run.head_repository").owner, "workflow_run.head_repository.owner");
  return {
    head_sha: string(run.head_sha, "workflow_run.head_sha"),
    head_branch: string(run.head_branch, "workflow_run.head_branch"),
    head_repository: { owner: { login: string(owner.login, "workflow_run.head_repository.owner.login") } },
  };
}

async function comment({ github, context }: Pick<Actions, "github" | "context">, file: string, log: Log): Promise<void> {
  const body = await readFile(file, "utf8");
  const marker = body.slice(0, body.indexOf("\n"));
  const run = workflowRun(context.payload);
  const { owner, repo } = context.repo;
  const pulls = await github.paginate(github.rest.pulls.list, {
    owner,
    repo,
    state: "open",
    head: `${run.head_repository.owner.login}:${run.head_branch}`,
    per_page: 100,
  });
  const pull = pulls.find((candidate) => candidate.head.sha === run.head_sha);
  if (!pull) {
    log(`No open pull request has ${run.head_sha} as its head.`);
    return;
  }
  log(`Pull request #${pull.number}`);
  const comments = await github.paginate(github.rest.issues.listComments, { owner, repo, issue_number: pull.number, per_page: 100 });
  const existing = comments.find((candidate) => candidate.user?.login === "github-actions[bot]" && candidate.body?.startsWith(marker));
  const { data } = existing
    ? await github.rest.issues.updateComment({ owner, repo, comment_id: existing.id, body })
    : await github.rest.issues.createComment({ owner, repo, issue_number: pull.number, body });
  log(data.html_url);
}

function expectArguments(args: string[], count: number): string[] {
  if (args.length !== count) throw new Error(USAGE);
  return args;
}

export default async function main({ core, github, context }: Partial<Actions>, argv: string[]): Promise<void> {
  const log: Log = core ? (message) => core.info(message) : console.log;
  const [subcommand, ...args] = argv;
  switch (subcommand) {
    case "setup": {
      const [builds] = expectArguments(args, 1);
      await setup(resolve(builds));
      return;
    }
    case "bench": {
      const [corpus, results, builds] = expectArguments(args, 3);
      await bench(resolve(corpus), resolve(results), resolve(builds));
      return;
    }
    case "report": {
      const [results, file] = expectArguments(args, 2);
      const markdown = await render(resolve(results));
      await mkdir(dirname(resolve(file)), { recursive: true });
      await writeFile(file, markdown);
      log(markdown);
      if (core) await core.summary.addRaw(markdown).write();
      return;
    }
    case "comment": {
      const [file] = expectArguments(args, 1);
      if (!github || !context) throw new Error("comment needs the github and context of actions/github-script");
      await comment({ github, context }, file, log);
      return;
    }
    default:
      throw new Error(USAGE);
  }
}

if (import.meta.main) await main({}, process.argv.slice(2));
