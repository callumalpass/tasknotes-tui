import { spawn } from "node:child_process";
import {
  conformanceMetadata as fallbackMetadata,
  executeConformanceOperation as fallbackExecute,
} from "../../mdbase-tasknotes/dist/conformance.js";

const bridgePath =
  process.env.TASKNOTES_TUI_BRIDGE_PATH ??
  "../tasknotes-tui/target/debug/tasknotes-spec-bridge";
const bridge = spawn(bridgePath, ["--stdio"], {
  cwd: new URL("..", import.meta.url),
  stdio: ["pipe", "pipe", "inherit"],
});

const pending = [];
let buffer = "";

function refBridge() {
  bridge.ref();
  bridge.stdin.ref?.();
  bridge.stdout.ref?.();
}

function unrefBridgeWhenIdle() {
  if (pending.length !== 0) return;
  bridge.stdin.unref?.();
  bridge.stdout.unref?.();
  bridge.unref();
}

function rejectPending(error) {
  while (pending.length > 0) {
    pending.shift().reject(error);
  }
}

bridge.stdout.setEncoding("utf8");
bridge.stdout.on("data", (chunk) => {
  buffer += chunk;
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline === -1) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const next = pending.shift();
    if (!next) continue;
    next.resolve(JSON.parse(line));
    unrefBridgeWhenIdle();
  }
});
bridge.once("error", rejectPending);
bridge.once("exit", (code, signal) => {
  rejectPending(
    new Error(`tasknotes-spec-bridge exited before replying (code=${code}, signal=${signal})`),
  );
});

function send(operation, input) {
  return new Promise((resolve, reject) => {
    refBridge();
    pending.push({ resolve, reject });
    bridge.stdin.write(`${JSON.stringify({ operation, input })}\n`);
  });
}

const localMetadata = await send("meta.claim", {}).then((response) => response.result);
const preferRust = process.env.TASKNOTES_TUI_BRIDGE_MODE === "rust";

export const metadata = preferRust
  ? localMetadata
  : {
      implementation: "tasknotes-tui",
      version: localMetadata.version,
      spec_version: localMetadata.spec_version,
      validation_modes: Array.from(
        new Set([...(localMetadata.validation_modes || []), ...(fallbackMetadata.validation_modes || [])]),
      ),
      profiles: Array.from(new Set([...(localMetadata.profiles || []), ...(fallbackMetadata.profiles || [])])),
      capabilities: Array.from(
        new Set([...(localMetadata.capabilities || []), ...(fallbackMetadata.capabilities || [])]),
      ),
    };

export async function execute(operation, input) {
  if (!preferRust) {
    return fallbackExecute(operation, input);
  }

  return send(operation, input);
}
