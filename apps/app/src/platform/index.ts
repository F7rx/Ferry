// Picks the engine for this runtime: desktop/mobile builds run inside Tauri;
// a plain browser runs the PWA engine (WebRTC through a signaling server).
// `?demo` (kept for the tab) shows the clearly labelled demo with simulated devices.
import { createDemoPlatform } from "./demo";
import { createNativePlatform } from "./native";
import { createWebPlatform } from "./web";
import type { Platform } from "./types";

export const isNative = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function wantsDemo(): boolean {
  if (typeof window === "undefined") return false;
  try {
    if (new URLSearchParams(location.search).has("demo")) sessionStorage.setItem("ferry.demo", "1");
    return sessionStorage.getItem("ferry.demo") === "1";
  } catch {
    return new URLSearchParams(location.search).has("demo");
  }
}

type NativePlatform = ReturnType<typeof createNativePlatform>;
type DemoPlatform = ReturnType<typeof createDemoPlatform>;
type WebPlatform = ReturnType<typeof createWebPlatform>;
type AnyPlatform = Platform & Partial<NativePlatform> & Partial<DemoPlatform> & Partial<WebPlatform>;

const isDemo = !isNative && wantsDemo();

export const platform: AnyPlatform = isNative ? createNativePlatform() : isDemo ? createDemoPlatform() : createWebPlatform();
export const native = isNative ? (platform as NativePlatform) : null;
export const demo = isDemo ? (platform as DemoPlatform) : null;
export const web = !isNative && !isDemo ? (platform as WebPlatform) : null;

export * from "./types";
