// Appearance preferences. Applied to <html> as data attributes that the
// tokens read; the inline script in index.html applies the same rules before
// first paint. Keep the storage key and rules in sync with it.
import { reactive, watch } from "vue";

export type Tri = "system" | "on" | "off";
export interface Appearance {
  theme: "system" | "light" | "dark";
  motion: "system" | "reduced" | "full";
  transparency: "system" | "reduced" | "full";
}

const KEY = "ferry.appearance";

function load(): Appearance {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) || "{}");
    return { theme: raw.theme ?? "system", motion: raw.motion ?? "system", transparency: raw.transparency ?? "system" };
  } catch {
    return { theme: "system", motion: "system", transparency: "system" };
  }
}

export const appearance = reactive<Appearance>(load());

const mq = (q: string) => (typeof window !== "undefined" && window.matchMedia ? window.matchMedia(q) : null);
const dark = mq("(prefers-color-scheme: dark)");
const reducedMotion = mq("(prefers-reduced-motion: reduce)");
const reducedTransparency = mq("(prefers-reduced-transparency: reduce)");
const moreContrast = mq("(prefers-contrast: more)");

export function apply() {
  const root = document.documentElement;
  const theme = appearance.theme === "system" ? (dark?.matches ? "dark" : "light") : appearance.theme;
  // Flip the palette in one frame: with transitions running, every surface
  // would fade on its own schedule and the switch would smear.
  if (root.dataset.theme && root.dataset.theme !== theme) snapTransitions();
  root.dataset.theme = theme;
  root.dataset.motion =
    appearance.motion === "reduced" || (appearance.motion === "system" && reducedMotion?.matches) ? "reduced" : "full";
  root.dataset.transparency =
    appearance.transparency === "reduced" || (appearance.transparency === "system" && reducedTransparency?.matches)
      ? "reduced"
      : "full";
  if (moreContrast?.matches) root.dataset.contrast = "more";
  else delete root.dataset.contrast;
  document.querySelector('meta[name="theme-color"]:not([media])')?.setAttribute("content", theme === "dark" ? "#000000" : "#f3f3f3");
}

function snapTransitions() {
  const style = document.createElement("style");
  style.textContent = "*,*::before,*::after{transition:none !important}";
  document.head.appendChild(style);
  void document.body.offsetHeight;
  requestAnimationFrame(() => style.remove());
}

export function initAppearance() {
  apply();
  for (const m of [dark, reducedMotion, reducedTransparency, moreContrast]) m?.addEventListener("change", apply);
  watch(appearance, () => {
    try {
      localStorage.setItem(KEY, JSON.stringify(appearance));
    } catch {
      /* private mode */
    }
    apply();
  });
}

export function motionReduced(): boolean {
  return document.documentElement.dataset.motion === "reduced";
}
