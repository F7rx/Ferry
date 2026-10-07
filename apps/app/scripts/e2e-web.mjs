// Browser ↔ browser end to end: two isolated Chrome profiles (separate
// identities and storage) running the production PWA build, a real
// ferry-signal process, and WebRTC between them. Everything through the UI:
// rename, pick files, send, accept, progress, Inbox, save, message back.
//
//   cargo build -p ferry-signal && node apps/app/scripts/e2e-web.mjs <screenshot dir>
//
// Runs from any directory. FERRY_SIGNAL_BIN overrides the server binary
// (default: target/debug/ferry-signal); FERRY_E2E_CHANNEL the browser (see
// e2e-lib.mjs). Exits 1 if any check fails.
import { createHash, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer, connect } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { buildPwa, exe, kill, launchBrowser, previewPwa, repoRoot, start } from "./e2e-lib.mjs";

const shots = process.argv[2] ?? "e2e-web";
mkdirSync(shots, { recursive: true });
const work = mkdtempSync(join(tmpdir(), "ferry-web-"));
const SIGNAL = 3917;
/** Size of the large test file; raise it to measure throughput (FERRY_WEB_MB=200). */
const BIG_MB = Number(process.env.FERRY_WEB_MB ?? 48);
const WEB = 4176;
const SIGNAL_BIN = resolve(process.env.FERRY_SIGNAL_BIN ?? join(repoRoot, "target", "debug", `ferry-signal${exe}`));
if (!existsSync(SIGNAL_BIN)) throw new Error(`missing ${SIGNAL_BIN}: cargo build -p ferry-signal`);
const sha = (path) => createHash("sha256").update(readFileSync(path)).digest("hex");
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (name, ok, detail = "") => {
  results.push(ok);
  log(ok ? "PASS" : "FAIL", name, detail);
};
async function until(fn, ms, what) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await fn().catch(() => false)) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out: ${what}`);
}

// The PWA, built against the local signaling server.
const dist = join(work, "dist");
log("building the PWA…");
buildPwa(dist, { VITE_FERRY_SIGNAL_URL: `ws://127.0.0.1:${SIGNAL}/v1/ws` });

// The server trusts X-Forwarded-For from loopback, so the proxy below can
// make a third browser look like it is on another network (203.0.113.7).
const signal = start(SIGNAL_BIN, ["--addr", `127.0.0.1:${SIGNAL}`], {
  stdio: "ignore",
  env: { ...process.env, FERRY_SIGNAL_TRUSTED_PROXIES: "127.0.0.1" },
});
const REMOTE = SIGNAL + 1;
const proxy = createServer((client) => {
  let head = Buffer.alloc(0);
  const onData = (chunk) => {
    head = Buffer.concat([head, chunk]);
    const end = head.indexOf("\r\n\r\n");
    if (end < 0) return;
    client.off("data", onData);
    const upstream = connect(SIGNAL, "127.0.0.1", () => {
      upstream.write(Buffer.concat([head.subarray(0, end), Buffer.from("\r\nX-Forwarded-For: 203.0.113.7"), head.subarray(end)]));
      client.pipe(upstream).pipe(client);
    });
    upstream.on("error", () => client.destroy());
    client.on("error", () => upstream.destroy());
  };
  client.on("data", onData);
}).listen(REMOTE, "127.0.0.1");
const preview = previewPwa(dist, WEB);
const browser = await launchBrowser();
const errors = [];

try {
  await until(async () => (await fetch(`http://127.0.0.1:${SIGNAL}/healthz`)).ok, 15000, "signaling server");
  await until(async () => (await fetch(`http://localhost:${WEB}/`)).ok, 15000, "preview server");

  async function device(alias, viewport) {
    const context = await browser.newContext({ viewport, acceptDownloads: true });
    // Lets the test cut this tab's WebRTC connections (resume scenario).
    await context.addInitScript(() => {
      const Original = window.RTCPeerConnection;
      window.__ferryPcs = [];
      window.RTCPeerConnection = class extends Original {
        constructor(...args) {
          super(...args);
          window.__ferryPcs.push(this);
        }
      };
    });
    const page = await context.newPage();
    page.on("pageerror", (e) => errors.push(`${alias}: ${e.message}`));
    page.on("console", (m) => m.type() === "error" && errors.push(`${alias}: ${m.text()}`));
    await page.goto(`http://localhost:${WEB}/settings/general`);
    const name = page.locator(".panel input.input").first();
    await name.waitFor();
    await name.fill(alias);
    await name.press("Enter");
    await page.getByText("Saved").first().waitFor({ timeout: 5000 });
    return { context, page };
  }

  const maya = await device("Maya's laptop", { width: 1440, height: 900 });
  const studio = await device("Studio browser", { width: 1280, height: 860 });

  // ── Discovery through the signaling server ─────────────────────────────
  await maya.page.goto(`http://localhost:${WEB}/`);
  await studio.page.goto(`http://localhost:${WEB}/`);
  const studioTile = maya.page.locator("[data-device-id]", { hasText: "Studio browser" }).first();
  await studioTile.waitFor({ timeout: 15000 });
  check("browsers find each other through ferry-signal", true);
  await maya.page.waitForTimeout(600);
  await maya.page.screenshot({ path: join(shots, "web-01-nearby.png") });

  // ── Files: Maya → Studio, accepted in Studio's prompt ──────────────────
  const small = join(work, "Brief.pdf");
  const big = join(work, "Rough cut.mp4");
  writeFileSync(small, randomBytes(1_200_000));
  writeFileSync(big, randomBytes(BIG_MB * 1024 * 1024));
  const [chooser] = await Promise.all([maya.page.waitForEvent("filechooser"), maya.page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser.setFiles([small, big]);
  await studioTile.click();
  await maya.page.getByRole("button", { name: "Send to Studio browser" }).click();

  const accept = studio.page.getByRole("button", { name: /^Accept$/ });
  await accept.waitFor({ timeout: 20000 });
  check("receiver is asked first", await studio.page.getByText("Maya's laptop").first().isVisible());
  await studio.page.screenshot({ path: join(shots, "web-02-incoming.png") });
  const started = Date.now();
  await accept.click();
  await maya.page.waitForTimeout(1500);
  await maya.page.screenshot({ path: join(shots, "web-03-sending.png") });
  await maya.page.getByText("Sent to Studio browser").first().waitFor({ timeout: 120000 });
  const secs = (Date.now() - started) / 1000;
  check("sender sees the transfer complete", true, `${((BIG_MB * 1.048576 + 1.2) / secs).toFixed(1)} MB/s over WebRTC, accept to done`);

  // ── Studio's Inbox: save both files and compare bytes ──────────────────
  await studio.page.goto(`http://localhost:${WEB}/inbox`);
  for (const [path, name] of [
    [small, "Brief.pdf"],
    [big, "Rough cut.mp4"],
  ]) {
    const save = studio.page.getByRole("button", { name: `Save ${name}` });
    await save.waitFor({ timeout: 10000 });
    const [download] = await Promise.all([studio.page.waitForEvent("download"), save.click()]);
    const out = join(work, `saved-${name}`);
    await download.saveAs(out);
    check(`${name} arrives byte-identical`, sha(out) === sha(path), download.suggestedFilename());
  }
  await studio.page.screenshot({ path: join(shots, "web-04-inbox.png") });

  // A peer-declared HTML file is never rendered on Ferry's origin: Open saves it instead.
  const page = join(work, "invoice.html");
  writeFileSync(page, "<script>document.title='pwned'</script><h1>hi</h1>");
  const [chooserH] = await Promise.all([maya.page.waitForEvent("filechooser"), maya.page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooserH.setFiles([page]);
  await studioTile.click();
  await maya.page.getByRole("button", { name: "Send to Studio browser" }).click();
  await studio.page.getByRole("button", { name: /^Accept$/ }).click({ timeout: 20000 });
  await maya.page.getByText("Sent to Studio browser").first().waitFor({ timeout: 30000 });
  await studio.page.goto(`http://localhost:${WEB}/inbox`);
  const openHtml = studio.page.locator("li.item", { hasText: "invoice.html" }).getByRole("button", { name: "Open", exact: true });
  await openHtml.waitFor({ timeout: 10000 });
  const pagesBefore = studio.context.pages().length;
  const [htmlDownload] = await Promise.all([studio.page.waitForEvent("download", { timeout: 10000 }), openHtml.click()]);
  check("received HTML is saved, not rendered", htmlDownload.suggestedFilename() === "invoice.html" && studio.context.pages().length === pagesBefore);

  // ── A message back: Studio → Maya (no prompt for messages) ─────────────
  await studio.page.goto(`http://localhost:${WEB}/`);
  await studio.page.getByRole("button", { name: "Text or link" }).click();
  await studio.page.locator("#compose-text").fill("Got it, looks great. https://example.com/notes");
  await studio.page.getByRole("button", { name: "Add", exact: true }).click();
  await studio.page.locator("[data-device-id]", { hasText: "Maya's laptop" }).first().click();
  await studio.page.getByRole("button", { name: "Send to Maya's laptop" }).click();
  const message = maya.page.getByRole("dialog", { name: "Message from Studio browser" });
  await message.waitFor({ timeout: 20000 });
  check("message arrives without a prompt", (await message.textContent())?.includes("looks great") ?? false);
  await maya.page.screenshot({ path: join(shots, "web-05-message.png") });

  // ── Decline ────────────────────────────────────────────────────────────
  const [chooser2] = await Promise.all([maya.page.waitForEvent("filechooser"), maya.page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser2.setFiles([small]);
  await studioTile.click();
  await maya.page.getByRole("button", { name: "Send to Studio browser" }).click();
  await studio.page.getByRole("button", { name: "Decline" }).click({ timeout: 20000 });
  await maya.page.getByText(/declined/i).first().waitFor({ timeout: 10000 });
  check("decline reaches the sender", true);

  // ── Resume: the connection drops mid-transfer ─────────────────────────
  const archive = join(work, "Archive.zip");
  writeFileSync(archive, randomBytes(120 * 1024 * 1024));
  await maya.page.goto(`http://localhost:${WEB}/`);
  await studioTile.waitFor({ timeout: 15000 });
  const [chooser5] = await Promise.all([maya.page.waitForEvent("filechooser"), maya.page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser5.setFiles([archive]);
  await studioTile.click();
  await maya.page.getByRole("button", { name: "Send to Studio browser" }).click();
  const acceptBig = studio.page.getByRole("button", { name: /^Accept$/ });
  await acceptBig.waitFor({ timeout: 20000 });
  await acceptBig.click();
  await studio.page.waitForTimeout(2500);
  const cut = await studio.page.evaluate(() => {
    const open = window.__ferryPcs.filter((pc) => pc.connectionState !== "closed");
    open.forEach((pc) => pc.close());
    return open.length;
  });
  check("connection cut mid-transfer", cut > 0, `${cut} peer connection(s) closed`);
  await maya.page.getByText(/reconnecting/i).first().waitFor({ timeout: 15000 });
  await maya.page.screenshot({ path: join(shots, "web-10-reconnecting.png") });
  await maya.page.getByText("Sent to Studio browser").first().waitFor({ timeout: 120000 });
  check("transfer resumes and completes without a second prompt", (await studio.page.getByRole("button", { name: /^Accept$/ }).count()) === 0);
  await studio.page.goto(`http://localhost:${WEB}/inbox`);
  const saveArchive = studio.page.getByRole("button", { name: "Save Archive.zip" });
  await saveArchive.waitFor({ timeout: 10000 });
  const [download4] = await Promise.all([studio.page.waitForEvent("download"), saveArchive.click()]);
  const out4 = join(work, "saved-archive.zip");
  await download4.saveAs(out4);
  check("resumed file is byte-identical", sha(out4) === sha(archive));
  check("resumed file is listed once", (await studio.page.getByRole("button", { name: "Save Archive.zip" }).count()) === 1);

  // ── A private link: a device on another network joins through it ──────
  const remote = await device("Travel phone", { width: 390, height: 844 });
  await remote.page.goto(`http://localhost:${WEB}/settings/network`);
  const signalInput = remote.page.locator(".panel input.input").first();
  await signalInput.fill(`ws://127.0.0.1:${REMOTE}/v1/ws`);
  await signalInput.press("Enter");
  await remote.page.getByText("Saved").first().waitFor({ timeout: 5000 });
  await remote.page.goto(`http://localhost:${WEB}/`);
  await remote.page.waitForTimeout(2500);
  check("another network sees no one nearby", (await remote.page.locator("[data-device-id]").count()) === 0);

  await maya.page.goto(`http://localhost:${WEB}/`);
  await maya.page.getByRole("button", { name: "Create a link" }).click();
  const linkEl = maya.page.locator("article[aria-label='Private link'] code");
  await linkEl.waitFor({ timeout: 5000 });
  const link = await linkEl.getAttribute("title");
  check("private link keeps its secret in the fragment", /^http:\/\/localhost:\d+\/#room=[A-Za-z0-9_-]{22}$/.test(link ?? ""), link ?? "");
  await remote.page.goto(link);
  const mayaTile = remote.page.locator("[data-device-id]", { hasText: "Maya's laptop" }).first();
  await mayaTile.waitFor({ timeout: 15000 });
  await maya.page.getByText("1 device connected").waitFor({ timeout: 10000 });
  check("devices meet through the link", true);
  check("only the link's devices are visible", (await remote.page.locator("[data-device-id]", { hasText: "Studio browser" }).count()) === 0);
  await remote.page.screenshot({ path: join(shots, "web-08-room-phone.png") });
  await maya.page.screenshot({ path: join(shots, "web-09-room-host.png") });

  const photo = join(work, "Boarding pass.png");
  writeFileSync(photo, randomBytes(2_400_000));
  const [chooser3] = await Promise.all([remote.page.waitForEvent("filechooser"), remote.page.getByRole("button", { name: "Files", exact: true }).click()]);
  await chooser3.setFiles([photo]);
  await mayaTile.click();
  await remote.page.getByRole("button", { name: "Send to Maya's laptop" }).click();
  const acceptRemote = maya.page.getByRole("button", { name: /^Accept$/ });
  await acceptRemote.waitFor({ timeout: 20000 });
  await acceptRemote.click();
  await remote.page.getByText("Sent to Maya's laptop").first().waitFor({ timeout: 30000 });
  await maya.page.goto(`http://localhost:${WEB}/inbox`);
  const saveRemote = maya.page.getByRole("button", { name: "Save Boarding pass.png" });
  await saveRemote.waitFor({ timeout: 10000 });
  const [download3] = await Promise.all([maya.page.waitForEvent("download"), saveRemote.click()]);
  const out3 = join(work, "saved-boarding.png");
  await download3.saveAs(out3);
  check("file sent through the link arrives byte-identical", sha(out3) === sha(photo));

  // What the browser build reports about itself.
  await maya.page.goto(`http://localhost:${WEB}/diagnostics`);
  await maya.page.getByText("Signaling server").first().waitFor();
  await maya.page.waitForTimeout(1500);
  await maya.page.screenshot({ path: join(shots, "web-06-diagnostics.png") });
  await maya.page.goto(`http://localhost:${WEB}/settings/network`);
  await maya.page.waitForTimeout(600);
  await maya.page.screenshot({ path: join(shots, "web-07-network.png") });

  check("no page errors", errors.length === 0, errors.slice(0, 3).join(" | "));
} catch (err) {
  check("scenario completed", false, String(err));
  for (const ctx of browser.contexts()) for (const [i, p] of ctx.pages().entries()) await p.screenshot({ path: join(shots, `web-99-failure-${i}.png`) }).catch(() => {});
  if (errors.length) log("page errors:", errors.slice(0, 5).join(" | "));
} finally {
  await browser.close();
  kill(preview);
  kill(signal);
  proxy.close();
}
const failed = results.filter((r) => !r).length;
log(`${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
