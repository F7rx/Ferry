// Shared plumbing for the end-to-end scripts that CI runs (e2e-web,
// e2e-native-web, e2e-pwa): paths that work from any working directory, the
// browser to launch, and child processes that are cleaned up on every exit
// path and on every platform.
//
// FERRY_E2E_CHANNEL picks the Playwright browser channel. The default,
// "chrome", is the installed Google Chrome (preinstalled on GitHub's Windows
// and Ubuntu runners). "chromium" uses Playwright's own build, installed with
// `npx playwright install chromium`.
import { chromium } from "@playwright/test";
import { spawn, spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

/** apps/app, where vite runs. */
export const appDir = fileURLToPath(new URL("..", import.meta.url));
/** The repository root (the Cargo workspace with target/). */
export const repoRoot = join(appDir, "..", "..");
export const exe = process.platform === "win32" ? ".exe" : "";

export function launchBrowser() {
  return chromium.launch({ channel: process.env.FERRY_E2E_CHANNEL || "chrome" });
}

const children = new Set();

/**
 * Starts a child process that is killed when the script exits. Shell commands
 * (npx ...) get their own process group outside Windows, so killing them also
 * stops the node process the shell started.
 */
export function start(command, args, options = {}) {
  const shell = !!options.shell;
  const opts = { ...options, detached: shell && process.platform !== "win32" };
  const child = shell ? spawn(command, opts) : spawn(command, args, opts);
  children.add(child);
  child.on("exit", () => children.delete(child));
  return child;
}

export function kill(child) {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"]);
    return;
  }
  try {
    process.kill(-child.pid, "SIGTERM");
  } catch {
    child.kill("SIGTERM");
  }
}

process.on("exit", () => {
  for (const child of children) kill(child);
});

/** Runs `npx vite build` in apps/app into `outDir`; throws with the output on failure. */
export function buildPwa(outDir, env = {}) {
  const build = spawnSync(`npx vite build --outDir "${outDir}" --emptyOutDir`, {
    shell: true,
    cwd: appDir,
    env: { ...process.env, ...env },
    encoding: "utf8",
  });
  if (build.status !== 0) throw new Error(`vite build failed\n${build.stdout}${build.stderr}`);
}

/** Serves a build with `vite preview` on a fixed port. */
export function previewPwa(outDir, port) {
  return start(`npx vite preview --port ${port} --strictPort --outDir "${outDir}"`, null, { shell: true, cwd: appDir, stdio: "ignore" });
}
