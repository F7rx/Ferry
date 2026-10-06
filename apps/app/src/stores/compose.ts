// What the user is about to send: staged items and chosen devices. Shared by
// Home's drop surface, the Send view, Quick Drop and the tray handoff.
import { computed, reactive } from "vue";
import { platform, type OutgoingItem } from "../platform";
import { attempt, store, toast } from "./engine";

export const compose = reactive({
  items: [] as (OutgoingItem & { key: string })[],
  targets: new Set<string>(),
  /** Files are being dragged over the window. */
  dragging: false,
  /** Device tile under the cursor during a drag (Quick Drop). */
  hoverTarget: null as string | null,
});

let key = 0;
export function stage(items: OutgoingItem[]) {
  for (const item of items) {
    const duplicate = compose.items.some(
      (i) => (i.kind === "path" && item.kind === "path" && i.path === item.path) || (i.kind === "text" && item.kind === "text" && i.text === item.text),
    );
    if (!duplicate) compose.items.push({ ...item, key: `i${key++}` });
  }
}

export function unstage(k: string) {
  compose.items = compose.items.filter((i) => i.key !== k);
}

export function clearCompose() {
  compose.items = [];
  compose.targets.clear();
}

export function toggleTarget(id: string) {
  if (compose.targets.has(id)) compose.targets.delete(id);
  else compose.targets.add(id);
}

export const stagedBytes = computed(() =>
  compose.items.reduce((n, i) => n + (i.kind === "path" ? (i.size ?? 0) : i.kind === "text" ? i.text.length : i.size), 0),
);
export const hasFolders = computed(() => compose.items.some((i) => i.kind === "path" && i.isDir));

/** Sends the staged items to `targets` (default: the selected devices). */
export async function sendStaged(targets?: string[]) {
  const ids = targets ?? [...compose.targets];
  if (!ids.length || !compose.items.length) return null;
  const items = compose.items.map(({ key: _k, ...rest }) => rest as OutgoingItem);
  const result = await attempt(() => platform.send(ids.map((id) => ({ kind: "device" as const, id })), items));
  if (result) clearCompose();
  return result;
}

/** Quick Drop: items straight to one device, bypassing the staging tray. */
export async function sendNow(deviceId: string, items: OutgoingItem[]) {
  if (!items.length) return null;
  const device = store.devices.get(deviceId);
  const result = await attempt(() => platform.send([{ kind: "device", id: deviceId }], items));
  if (result && device && !device.online) {
    toast({ level: "info", title: `${device.alias} looks offline`, body: "Ferry will keep trying for a while." });
  }
  return result;
}
