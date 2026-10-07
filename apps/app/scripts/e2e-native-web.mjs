// Browser ↔ native end to end over WebRTC: real Chrome running the production
// PWA build, a real ferry-signal process and the real `ferry` CLI (native
// ferry-dc/1 on webrtc-rs). Browser → CLI files, CLI → browser files, messages
// both ways, declines both ways, byte-identical checks and throughput in both
// directions.
//
//   cargo build --release -p ferry-cli -p ferry-signal
//   node apps/app/scripts/e2e-native-web.mjs [screenshot dir]
//
// Runs from any directory. FERRY_BIG_MB (default 110) sets the size of the
// large file in each direction; FERRY_BIN / FERRY_SIGNAL_BIN override the
// binaries (default: target/release); FERRY_E2E_CHANNEL the browser (see
// e2e-lib.mjs). Exits 1 if any check fails.
import { createHash, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, writeFileSync, createReadStream } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import readline from "node:readline";
import { buildPwa, exe, kill, launchBrowser, previewPwa, repoRoot, start } from "./e2e-lib.mjs";

const shots = process.argv[2] ?? "e2e-native-web";
mkdirSync(shots, { recursive: true });
const work = mkdtempSync(join(tmpdir(), "ferry-native-web-"));
const SIGNAL = 39417;
const WEB = 39418;
const BIG_MB = Number(process.env.FERRY_BIG_MB ?? 110);
const FERRY = resolve(process.env.FERRY_BIN ?? join(repoRoot, "target", "release", `ferry${exe}`));
const SIGNAL_BIN = resolve(process.env.FERRY_SIGNAL_BIN ?? join(repoRoot, "target", "release", `ferry-signal${exe}`));
const URL_WS = `ws://127.0.0.1:${SIGNAL}/v1/ws`;

const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (name, ok, detail = "") => {
  results.push(ok);
  log(ok ? "PASS" : "FAIL", name, detail);
};
const children = new Set();
async function until(fn, ms, what) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await fn().catch(() => false)) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out: ${what}`);
}
function sha(path) {
  return new Promise((res, rej) => {
    const h = createHash("sha256");
    createReadStream(path).on("data", (d) => h.update(d)).on("end", () => res(h.digest("hex"))).on("error", rej);
  });
}
function bigFile(path, mb) {
  // Random 1 MiB blocks: incompressible, cheap to generate.
  const block = randomBytes(1024 * 1024);
  const parts = [];
  for (let i = 0; i < mb; i++) {
    block.writeUInt32LE(i, 0);
    parts.push(Buffer.from(block));
  }
  writeFileSync(path, Buffer.concat(parts));
}
const mbps = (bytes, ms) => ((bytes / 1e6) / (ms / 1000)).toFixed(1);

/** Runs the CLI with --json; collects timestamped events. */
function ferry(args, name) {
  const child = start(FERRY, ["--ephemeral", "--json", "--signal", URL_WS, ...args], { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } });
  children.add(child);
  const events = [];
  const waiters = new Set();
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      return;
    }
    ev.at = Date.now();
    events.push(ev);
    for (const w of [...waiters]) if (w.pred(ev)) (waiters.delete(w), w.resolve(ev));
  });
  let stderr = "";
  child.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => child.on("exit", (code) => r(code)));
  return {
    child,
    events,
    exited,
    stderr: () => stderr,
    name,
    wait(pred, ms, what) {
      const hit = events.find(pred);
      if (hit) return Promise.resolve(hit);
      return new Promise((res, rej) => {
        const w = { pred, resolve: res };
        waiters.add(w);
        setTimeout(() => (waiters.delete(w), rej(new Error(`timed out: ${what} (${name}) ${stderr.slice(-400)}`))), ms);
      });
    },
  };
}
/** Throughput of a transfer from its JSON events: first bytes → completed. */
function rate(events, id) {
  const mine = events.filter((e) => e.type === "transferUpdated" && e.transfer.id === id);
  const first = mine.find((e) => e.transfer.bytesDone > 0);
  const done = mine.find((e) => e.transfer.state === "completed");
  if (!first || !done) return null;
  return { bytes: done.transfer.totalBytes, ms: done.at - first.at };
}

for (const bin of [FERRY, SIGNAL_BIN]) if (!existsSync(bin)) throw new Error(`missing ${bin}: cargo build --release -p ferry-cli -p ferry-signal`);

// The PWA, built against the local signaling server.
const dist = join(work, "dist");
log("building the PWA…");
buildPwa(dist, { VITE_FERRY_SIGNAL_URL: URL_WS });

const signal = start(SIGNAL_BIN, ["--addr", `127.0.0.1:${SIGNAL}`], { stdio: "ignore" });
children.add(signal);
children.add(previewPwa(dist, WEB));
const browser = await launchBrowser();
const errors = [];
const nativeIn = join(work, "native-in");
const strictIn = join(work, "strict-in");
mkdirSync(nativeIn);
mkdirSync(strictIn);

try {
  await until(async () => (await fetch(`http://127.0.0.1:${SIGNAL}/healthz`)).ok, 15000, "signaling server");
  await until(async () => (await fetch(`http://localhost:${WEB}/`)).ok, 15000, "preview server");

  const receiver = ferry(["--alias", "Native PC", "--port", "39421", "receive", "--accept", "all", "--dir", nativeIn], "receiver");
  await receiver.wait((e) => e.type === "ready", 20000, "receiver ready");

  const context = await browser.newContext({ viewport: { width: 1360, height: 880 }, acceptDownloads: true });
  const page = await context.newPage();
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  await page.goto(`http://localhost:${WEB}/settings/general`);
  const name = page.locator(".panel input.input").first();
  await name.waitFor();
  await name.fill("Web browser");
  await name.press("Enter");
  await page.getByText("Saved").first().waitFor({ timeout: 5000 });
  await page.goto(`http://localhost:${WEB}/`);

  // ── The CLI shows up in the browser (and the browser in the CLI) ──────
  const nativeTile = page.locator("[data-device-id]", { hasText: "Native PC" }).first();
  await nativeTile.waitFor({ timeout: 20000 });
  check("browser sees the native device through ferry-signal", true);
  await receiver.wait((e) => e.type === "deviceUpdated" && e.device.alias === "Web browser" && e.device.id.startsWith("rtc:"), 20000, "browser listed in CLI");
  check("native device lists the browser as an rtc: device", true);
  await page.screenshot({ path: join(shots, "nw-01-nearby.png") });

  // ── Browser → CLI: files ───────────────────────────────────────────────
  const small = join(work, "Brief.pdf");
  const big = join(work, "Footage.mov");
  writeFileSync(small, randomBytes(1_200_000));
  log(`writing ${BIG_MB} MiB test files…`);
  bigFile(big, BIG_MB);
  const [chooser] = await Promise.all([page.waitForEvent("filechooser"), page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser.setFiles([small, big]);
  await nativeTile.click();
  await page.getByRole("button", { name: "Send to Native PC" }).click();
  const got = await receiver.wait((e) => e.type === "transferUpdated" && e.transfer.direction === "receive" && e.transfer.state === "completed" && e.transfer.fileCount === 2, 300000, "browser → CLI transfer");
  await page.getByText("Sent to Native PC").first().waitFor({ timeout: 30000 });
  check("browser sees its transfer complete", true);
  const up = rate(receiver.events, got.transfer.id);
  check("browser → CLI: connection reported as WebRTC", got.transfer.connection?.transport === "webrtc", JSON.stringify(got.transfer.connection));
  check("browser → CLI: Brief.pdf byte-identical", (await sha(join(nativeIn, "Brief.pdf"))) === (await sha(small)));
  check("browser → CLI: Footage.mov byte-identical", (await sha(join(nativeIn, "Footage.mov"))) === (await sha(big)), up ? `${mbps(up.bytes, up.ms)} MB/s (${(up.bytes / 1048576).toFixed(0)} MiB in ${(up.ms / 1000).toFixed(1)} s)` : "");
  check("no .ferrypart left on the CLI side", !readdirSync(nativeIn).some((f) => f.endsWith(".ferrypart")), readdirSync(nativeIn).join(", "));
  await page.screenshot({ path: join(shots, "nw-02-sent.png") });

  // ── Browser → CLI: a message ───────────────────────────────────────────
  await page.getByRole("button", { name: "Text or link" }).click();
  await page.locator("#compose-text").fill("Hello native, from Chrome");
  await page.getByRole("button", { name: "Add", exact: true }).click();
  await nativeTile.click();
  await page.getByRole("button", { name: "Send to Native PC" }).click();
  const msg = await receiver.wait((e) => e.type === "incomingRequest" && e.request.text != null, 30000, "message at CLI");
  check("browser → CLI message arrives without a prompt", msg.request.text === "Hello native, from Chrome" && msg.request.peer.alias === "Web browser");

  // ── CLI → browser: files ───────────────────────────────────────────────
  const big2 = join(work, "Dataset.bin");
  const note = join(work, "notes.txt");
  bigFile(big2, BIG_MB);
  writeFileSync(note, "native → browser ✓\n");
  const sender = ferry(["--alias", "Native sender", "--port", "39422", "send", big2, note, "--to", "Web browser", "--wait", "20"], "sender");
  const accept = page.getByRole("button", { name: /^Accept$/ });
  await accept.waitFor({ timeout: 30000 });
  check("browser is asked before receiving from the CLI", await page.getByText("Native sender").first().isVisible());
  await page.screenshot({ path: join(shots, "nw-03-incoming.png") });
  await accept.click();
  const code = await Promise.race([sender.exited, new Promise((r) => setTimeout(() => r("timeout"), 300000))]);
  check("CLI sender exits successfully", code === 0, `exit ${code} ${sender.stderr().slice(-300)}`);
  const sent = sender.events.filter((e) => e.type === "transferUpdated").at(-1);
  const down = sent ? rate(sender.events, sent.transfer.id) : null;
  check("CLI → browser: completed over WebRTC", sent?.transfer.state === "completed" && sent.transfer.connection?.transport === "webrtc", sent?.transfer.state);
  await page.goto(`http://localhost:${WEB}/inbox`);
  for (const [path, file] of [
    [big2, "Dataset.bin"],
    [note, "notes.txt"],
  ]) {
    const save = page.getByRole("button", { name: `Save ${file}` });
    await save.waitFor({ timeout: 30000 });
    const [download] = await Promise.all([page.waitForEvent("download", { timeout: 120000 }), save.click()]);
    const out = join(work, `saved-${file}`);
    await download.saveAs(out);
    const detail = file === "Dataset.bin" && down ? `${mbps(down.bytes, down.ms)} MB/s (${(down.bytes / 1048576).toFixed(0)} MiB in ${(down.ms / 1000).toFixed(1)} s)` : "";
    check(`CLI → browser: ${file} byte-identical`, (await sha(out)) === (await sha(path)), detail);
  }
  await page.screenshot({ path: join(shots, "nw-04-inbox.png") });

  // ── CLI → browser: a message ───────────────────────────────────────────
  await page.goto(`http://localhost:${WEB}/`);
  const texter = ferry(["--alias", "Native sender", "--port", "39423", "send", "--text", "Hello browser, from the CLI", "--to", "Web browser", "--wait", "20"], "texter");
  const dialog = page.getByRole("dialog", { name: "Message from Native sender" });
  await dialog.waitFor({ timeout: 30000 });
  check("CLI → browser message arrives without a prompt", (await dialog.textContent())?.includes("Hello browser, from the CLI") ?? false);
  check("CLI message sender exits successfully", (await texter.exited) === 0, texter.stderr().slice(-200));
  await page.screenshot({ path: join(shots, "nw-05-message.png") });

  // ── Declines both ways ─────────────────────────────────────────────────
  const declined = ferry(["--alias", "Native sender", "--port", "39424", "send", note, "--to", "Web browser", "--wait", "20"], "declined");
  await page.getByRole("button", { name: "Decline" }).click({ timeout: 30000 });
  const dcode = await declined.exited;
  const last = declined.events.filter((e) => e.type === "transferUpdated").at(-1);
  check("browser declines the CLI's offer", dcode === 1 && last?.transfer.state === "declined", `exit ${dcode}, state ${last?.transfer.state}`);

  const strict = ferry(["--alias", "Strict PC", "--port", "39425", "receive", "--accept", "trusted", "--dir", strictIn], "strict");
  await strict.wait((e) => e.type === "ready", 20000, "strict ready");
  const strictTile = page.locator("[data-device-id]", { hasText: "Strict PC" }).first();
  await strictTile.waitFor({ timeout: 20000 });
  const [chooser2] = await Promise.all([page.waitForEvent("filechooser"), page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser2.setFiles([small]);
  await strictTile.click();
  await page.getByRole("button", { name: "Send to Strict PC" }).click();
  await page.getByText("Strict PC declined").first().waitFor({ timeout: 30000 });
  check("CLI declines the browser's offer (untrusted)", readdirSync(strictIn).length === 0);
  await page.screenshot({ path: join(shots, "nw-06-declined.png") });

  check("no page errors", errors.length === 0, errors.slice(0, 3).join(" | "));
  if (up && down) log(`throughput: browser → CLI ${mbps(up.bytes, up.ms)} MB/s, CLI → browser ${mbps(down.bytes, down.ms)} MB/s (${BIG_MB} MiB each)`);
} catch (err) {
  check("scenario completed", false, String(err));
  for (const ctx of browser.contexts()) for (const [i, p] of ctx.pages().entries()) await p.screenshot({ path: join(shots, `nw-99-failure-${i}.png`) }).catch(() => {});
  if (errors.length) log("page errors:", errors.slice(0, 5).join(" | "));
} finally {
  await browser.close();
  for (const c of children) kill(c);
}
const failed = results.filter((r) => !r).length;
log(`${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
