// Renders the PWA icons from public/icon.svg with Chrome (no image toolchain
// needed): any-purpose 192/512, a full-bleed maskable 512 (the mark inside
// the 80% safe zone) and a full-bleed 180 apple-touch icon.
//   node scripts/icons.mjs
import { chromium } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";

const svg = readFileSync("public/icon.svg", "utf8");
const mark = svg.replace(/<rect[^>]*\/>/, ""); // the drawing without its rounded tile
const fullBleed = (scale) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" fill="#0a0a0a"/><g transform="translate(256 256) scale(${scale}) translate(-256 -256)">${mark
    .replace(/<svg[^>]*>/, "")
    .replace("</svg>", "")}</g></svg>`;

const outputs = [
  { file: "public/icons/icon-192.png", size: 192, svg },
  { file: "public/icons/icon-512.png", size: 512, svg },
  { file: "public/icons/maskable-512.png", size: 512, svg: fullBleed(0.78) },
  { file: "public/icons/apple-touch-icon.png", size: 180, svg: fullBleed(0.9) },
];

mkdirSync("public/icons", { recursive: true });
const browser = await chromium.launch({ channel: "chrome" });
for (const o of outputs) {
  const page = await browser.newPage({ viewport: { width: o.size, height: o.size }, deviceScaleFactor: 1 });
  const src = `data:image/svg+xml;base64,${Buffer.from(o.svg).toString("base64")}`;
  await page.setContent(`<html><body style="margin:0;background:transparent"><img src="${src}" width="${o.size}" height="${o.size}" style="display:block"></body></html>`);
  await page.waitForFunction(() => document.images[0]?.complete);
  await page.screenshot({ path: o.file, omitBackground: true });
  await page.close();
  console.log(o.file);
}
await browser.close();
