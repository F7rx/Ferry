// Browser links: one-off pages any browser on the same network can open to
// download what you shared, or to send files to this device.
import { computed, reactive } from "vue";
import { platform, type OutgoingItem } from "../platform";
import { attempt, store } from "./engine";

/** PINs we generated for our own links (the engine never echoes them back). */
export const linkPins = reactive(new Map<string, string>());

export const links = computed(() => [...store.links.values()].sort((a, b) => b.expiresAtMs - a.expiresAtMs));
export const downloadLinks = computed(() => links.value.filter((l) => l.kind === "download"));
export const uploadLinks = computed(() => links.value.filter((l) => l.kind === "upload"));

export function randomPin() {
  const n = crypto.getRandomValues(new Uint32Array(1))[0]! % 1_000_000;
  return String(n).padStart(6, "0");
}

export async function shareByLink(items: OutgoingItem[], pin: string | null = null) {
  const link = await attempt(() => platform.shareWithBrowsers(items, pin));
  if (link) {
    store.links.set(link.id, link);
    if (pin) linkPins.set(link.id, pin);
  }
  return link;
}

export async function receiveByLink(pin: string | null = null) {
  const link = await attempt(() => platform.receiveFromBrowsers(pin));
  if (link) {
    store.links.set(link.id, link);
    if (pin) linkPins.set(link.id, pin);
  }
  return link;
}

export async function stopLink(id: string) {
  await attempt(() => platform.stopBrowserLink(id));
  store.links.delete(id);
  linkPins.delete(id);
}
