// PWA checks on the production build: installable manifest, service worker in
// control, Web Share Target (the POST an OS share sheet would make, handled by
// the service worker and staged by the app), and the offline app shell.
//
//   node apps/app/scripts/e2e-pwa.mjs <screenshot dir>
//
// Builds the PWA itself and runs from any directory. FERRY_E2E_CHANNEL picks
// the browser (see e2e-lib.mjs). Exits 1 if any check fails.
import { mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:http";
import { buildPwa, kill, launchBrowser, previewPwa } from "./e2e-lib.mjs";

const shots = process.argv[2] ?? "e2e-pwa";
mkdirSync(shots, { recursive: true });
const work = mkdtempSync(join(tmpdir(), "ferry-pwa-"));
const PORT = 4177;
const base = `http://localhost:${PORT}`;
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (name, ok, detail = "") => {
  results.push(ok);
  log(ok ? "PASS" : "FAIL", name, detail);
};

const dist = join(work, "dist");
buildPwa(dist);
const preview = previewPwa(dist, PORT);
const browser = await launchBrowser();
const errors = [];

try {
  for (let i = 0; i < 50; i++) {
    if (await fetch(base).then((r) => r.ok, () => false)) break;
    await new Promise((r) => setTimeout(r, 200));
  }
  const context = await browser.newContext({ viewport: { width: 1280, height: 860 } });
  const page = await context.newPage();
  page.on("pageerror", (e) => errors.push(e.message));
  // Console errors too (a CSP violation is only logged), except the expected
  // WebSocket failures: no signaling server runs in this test.
  page.on("console", (m) => m.type() === "error" && !/WebSocket/i.test(m.text()) && errors.push(m.text()));
  await page.goto(base);

  // ── Manifest ─────────────────────────────────────────────────────────
  const href = await page.locator('link[rel="manifest"]').getAttribute("href");
  const manifest = await (await fetch(new URL(href, base))).json();
  const sizes = (manifest.icons ?? []).map((i) => i.sizes);
  check(
    "manifest is installable",
    !!manifest.name && manifest.display === "standalone" && !!manifest.start_url && sizes.includes("192x192") && sizes.includes("512x512"),
    `${manifest.name} · ${manifest.display} · icons ${sizes.join(", ")}`,
  );
  const icons = await Promise.all(
    (manifest.icons ?? []).map(async (i) => {
      const res = await fetch(new URL(i.src, base));
      return res.ok && (res.headers.get("content-type") ?? "").startsWith("image/");
    }),
  );
  check("every manifest icon loads", icons.length > 0 && icons.every(Boolean), `${icons.filter(Boolean).length}/${icons.length}`);
  check("manifest declares a share target", manifest.share_target?.action === "/share-target" && manifest.share_target?.method === "POST");

  // ── Service worker ───────────────────────────────────────────────────
  await page.evaluate(() => navigator.serviceWorker.ready);
  await page.reload();
  const controlled = await page.evaluate(() => !!navigator.serviceWorker.controller);
  check("service worker controls the page", controlled);

  // ── Share target: what the OS share sheet posts ──────────────────────
  const status = await page.evaluate(async () => {
    const form = new FormData();
    form.append("title", "Trip notes");
    form.append("text", "Meet at gate B12");
    form.append("files", new File([new Uint8Array(4096).fill(7)], "Shared photo.jpg", { type: "image/jpeg" }));
    const res = await fetch("/share-target", { method: "POST", body: form });
    return { status: res.status, url: res.url };
  });
  check("share POST is handled offline-first and redirects into the app", status.status === 200 && status.url.endsWith("/?shared=1"), JSON.stringify(status));
  await page.goto(`${base}/?shared=1`);
  await page.getByText("Shared photo.jpg").first().waitFor({ timeout: 8000 });
  const textStaged = await page.getByText("Meet at gate B12").first().isVisible().catch(() => false);
  check("shared file and text are staged for sending", textStaged);
  check("share query is cleaned from the address bar", !page.url().includes("shared=1"), page.url());
  await page.screenshot({ path: join(shots, "pwa-01-shared.png") });

  // Another website can't push items into the share tray (it would send a referrer).
  const evil = createServer((_, res) => {
    res.setHeader("content-type", "text/html");
    res.end(`<form method="post" enctype="multipart/form-data" action="${base}/share-target"><input name="text" value="injected"></form><script>document.forms[0].submit()</script>`);
  }).listen(PORT + 2, "127.0.0.1");
  const other = await context.newPage();
  await other.goto(`http://127.0.0.1:${PORT + 2}/`);
  await other.waitForURL(/share-target|shared/, { timeout: 8000 }).catch(() => {});
  const body = await other.content();
  check("cross-site share POST is refused", body.includes("Forbidden") && !other.url().includes("shared=1"), other.url());
  await other.close();
  evil.close();

  // ── Offline shell ────────────────────────────────────────────────────
  await context.setOffline(true);
  await page.goto(`${base}/devices`);
  await page.getByRole("heading", { name: "Devices" }).waitFor({ timeout: 8000 });
  check("app shell loads offline (deep link)", true);
  await page.goto(base);
  await page.getByText("Drop anything.").waitFor({ timeout: 8000 });
  check("home loads offline", true);
  await page.screenshot({ path: join(shots, "pwa-02-offline.png") });
  await context.setOffline(false);

  check("no page errors", errors.length === 0, errors.slice(0, 3).join(" | "));
} catch (err) {
  check("scenario completed", false, String(err));
  for (const ctx of browser.contexts()) for (const [i, p] of ctx.pages().entries()) await p.screenshot({ path: join(shots, `pwa-99-failure-${i}.png`) }).catch(() => {});
} finally {
  await browser.close();
  kill(preview);
}
const failed = results.filter((r) => !r).length;
log(`${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
