import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const wrangler = fileURLToPath(
  new URL("../node_modules/wrangler/bin/wrangler.js", import.meta.url),
);

const child = spawn(
  process.execPath,
  [wrangler, "deploy", "--dry-run", "--outdir", "dist"],
  {
    cwd: fileURLToPath(new URL("..", import.meta.url)),
    env: {
      ...process.env,
      WRANGLER_SEND_METRICS: "false",
    },
    stdio: ["ignore", "pipe", "pipe"],
  },
);

let completed = false;
let terminatedAfterSuccess = false;

const forceStop = setTimeout(() => {
  if (completed) return;
  child.kill("SIGKILL");
  process.stderr.write("Wrangler dry-run timed out after 90 seconds.\n");
  process.exitCode = 1;
}, 90_000);

function forward(stream, output) {
  stream.on("data", (chunk) => {
    output.write(chunk);
    if (completed || !chunk.toString().includes("--dry-run: exiting now.")) return;
    completed = true;
    clearTimeout(forceStop);
    setTimeout(() => {
      if (child.exitCode === null) {
        terminatedAfterSuccess = true;
        child.kill("SIGTERM");
      }
    }, 100);
  });
}

forward(child.stdout, process.stdout);
forward(child.stderr, process.stderr);

child.on("error", (error) => {
  clearTimeout(forceStop);
  process.stderr.write(`Unable to start Wrangler: ${error.message}\n`);
  process.exitCode = 1;
});

child.on("exit", (code, signal) => {
  clearTimeout(forceStop);
  if (completed && (code === 0 || terminatedAfterSuccess || signal === "SIGTERM")) {
    process.exitCode = 0;
    return;
  }
  process.exitCode = code ?? 1;
});
