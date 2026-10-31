declare module "monaco-editor/*?worker" {
  export default function createWorker(options?: { name?: string; inject?: string }): Worker;
}
