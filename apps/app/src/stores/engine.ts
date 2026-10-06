// The UI's view of the engine: a reactive mirror fed by engine events, plus
// actions that report failures as readable toasts.
import { computed, reactive } from "vue";
import { platform } from "../platform";
import type {
  BrowserLink,
  OutgoingPairing,
  PairingOffer,
  PairingRequest,
  RoomInfo,
  SignalingStatus,
  Decision,
  DeviceSummary,
  EngineEvent,
  HistoryEntry,
  IncomingRequest,
  LocalDevice,
  Settings,
  TransferFile,
  TransferSummary,
} from "../platform";

export interface Toast {
  id: number;
  level: "info" | "success" | "warning" | "error";
  title: string;
  body?: string;
  action?: { label: string; run: () => void };
}

export const store = reactive({
  ready: false,
  local: null as LocalDevice | null,
  devices: new Map<string, DeviceSummary>(),
  transfers: new Map<string, TransferSummary>(),
  /** Per-file details, kept only for transfers the user expanded. */
  files: new Map<string, TransferFile[]>(),
  requests: [] as IncomingRequest[],
  messages: [] as IncomingRequest[],
  history: [] as HistoryEntry[],
  settings: null as Settings | null,
  server: { running: true, port: 53317, error: null as string | null },
  links: new Map<string, BrowserLink>(),
  /** Private link rooms. */
  rooms: new Map<string, RoomInfo>(),
  /** WebRTC signaling connection (null until known). */
  signaling: null as SignalingStatus | null,
  pairing: {
    /** Code-comparison prompts from other devices, oldest first. */
    requests: [] as PairingRequest[],
    /** The QR code this device is showing, if any. */
    offer: null as PairingOffer | null,
    /** A code-comparison request this device sent and is waiting on. */
    outgoing: null as OutgoingPairing | null,
  },
  toasts: [] as Toast[],
});

let toastId = 1;
export function toast(t: Omit<Toast, "id">, ms = 5200) {
  const id = toastId++;
  store.toasts.push({ ...t, id });
  if (ms > 0) setTimeout(() => dismissToast(id), ms);
  return id;
}
export function dismissToast(id: number) {
  const i = store.toasts.findIndex((t) => t.id === id);
  if (i >= 0) store.toasts.splice(i, 1);
}

function errorText(err: unknown): { title: string; body?: string } {
  if (err && typeof err === "object" && "message" in err) {
    const e = err as { message: string; hint?: string };
    return { title: e.message, body: e.hint };
  }
  return { title: String(err) };
}

/** Runs an action; shows a toast instead of throwing. */
export async function attempt<T>(fn: () => Promise<T>): Promise<T | undefined> {
  try {
    return await fn();
  } catch (err) {
    toast({ level: "error", ...errorText(err) });
    return undefined;
  }
}

function handle(event: EngineEvent) {
  switch (event.type) {
    case "localDeviceChanged":
      store.local = event.device;
      break;
    case "deviceUpdated":
      store.devices.set(event.device.id, event.device);
      break;
    case "deviceRemoved":
      store.devices.delete(event.id);
      break;
    case "incomingRequest":
      if (event.request.text != null) store.messages.unshift(event.request);
      else store.requests.push(event.request);
      break;
    case "incomingRequestClosed":
      store.requests = store.requests.filter((r) => r.id !== event.id);
      break;
    case "transferUpdated": {
      const prev = store.transfers.get(event.transfer.id);
      store.transfers.set(event.transfer.id, event.transfer);
      if (prev && !isFinal(prev.state) && isFinal(event.transfer.state)) onFinished(event.transfer);
      break;
    }
    case "transferFilesUpdated": {
      const list = store.files.get(event.id);
      if (list) {
        for (const f of event.files) {
          const i = list.findIndex((x) => x.id === f.id);
          if (i >= 0) list[i] = f;
        }
      }
      break;
    }
    case "transferRemoved":
      store.transfers.delete(event.id);
      store.files.delete(event.id);
      break;
    case "historyAdded":
      store.history.unshift(event.entry);
      if (store.history.length > 400) store.history.length = 400;
      break;
    case "serverStatus":
      store.server = { running: event.running, port: event.port, error: event.error };
      break;
    case "browserShareUpdated":
      store.links.set(event.share.id, event.share);
      break;
    case "browserShareRemoved":
      store.links.delete(event.id);
      break;
    case "pairingOfferClosed":
      if (store.pairing.offer?.id === event.id) store.pairing.offer = null;
      if (event.device) {
        store.devices.set(event.device.id, event.device);
        toast({ level: "success", title: `${displayName(event.device)} is now one of your devices`, body: "Files between them arrive without asking." }, 5200);
      }
      break;
    case "pairingRequest":
      store.pairing.requests.push(event.request);
      break;
    case "pairingRequestClosed":
      store.pairing.requests = store.pairing.requests.filter((r) => r.id !== event.id);
      break;
    case "pairingFinished": {
      const mine = store.pairing.outgoing?.id === event.id;
      const name = event.device ? displayName(event.device) : (store.pairing.outgoing?.peer.alias ?? "The other device");
      if (mine) store.pairing.outgoing = null;
      if (event.device) store.devices.set(event.device.id, event.device);
      if (event.outcome === "paired") toast({ level: "success", title: `${name} is now one of your devices`, body: "Files between them arrive without asking." }, 5200);
      if (event.outcome === "declined") toast({ level: "warning", title: `${name} didn't confirm`, body: "If the codes didn't match, someone else may be on this network. Try again on a network you trust." }, 9000);
      if (event.outcome === "failed" && event.error) toast({ level: "error", title: event.error.message, body: event.error.hint }, 9000);
      break;
    }
    case "roomUpdated":
      store.rooms.set(event.room.id, event.room);
      break;
    case "roomRemoved":
      store.rooms.delete(event.id);
      break;
    case "signalingStatus":
      store.signaling = event.status;
      break;
    case "notice":
      toast({ level: event.level === "error" ? "error" : event.level === "warning" ? "warning" : "info", title: event.message }, 9000);
      break;
  }
}

export function isFinal(state: TransferSummary["state"]) {
  return ["completed", "completedWithErrors", "declined", "cancelled", "failed"].includes(state);
}

function onFinished(t: TransferSummary) {
  if (t.state === "completed" && t.direction === "send") {
    toast({ level: "success", title: `Sent to ${t.peer.alias}`, body: t.fileCount > 1 ? `${t.fileCount} items` : t.title }, 3200);
  }
  if (t.state === "failed" && t.error) {
    toast({ level: "error", title: t.error.message, body: t.error.hint }, 9000);
  }
}

export async function initEngine() {
  platform.subscribe(handle);
  const snap = await platform.init();
  store.local = snap.local;
  for (const d of snap.devices) store.devices.set(d.id, d);
  for (const t of snap.transfers) store.transfers.set(t.id, t);
  store.settings = snap.settings;
  store.server = snap.server;
  store.history = (await platform.history(120).catch(() => [])) ?? [];
  for (const l of (await platform.browserLinks().catch(() => [])) ?? []) store.links.set(l.id, l);
  for (const r of (await platform.rooms().catch(() => [])) ?? []) store.rooms.set(r.id, r);
  store.signaling = (await platform.signalingStatus().catch(() => null)) ?? null;
  store.ready = true;
  return snap as typeof snap & { pendingPaths?: string[] };
}

// ── Derived views ─────────────────────────────────────────────────────────

export const devices = computed(() =>
  [...store.devices.values()].sort(
    (a, b) =>
      Number(b.online) - Number(a.online) ||
      Number(b.mine) - Number(a.mine) ||
      Number(b.favorite) - Number(a.favorite) ||
      displayName(a).localeCompare(displayName(b)),
  ),
);
export const onlineDevices = computed(() => devices.value.filter((d) => d.online));
export const quickDevices = computed(() => devices.value.filter((d) => d.favorite || d.mine));

export const transfers = computed(() => [...store.transfers.values()].sort((a, b) => b.startedAtMs - a.startedAtMs));
export const activeTransfers = computed(() => transfers.value.filter((t) => !isFinal(t.state)));

export function displayName(d: Pick<DeviceSummary, "alias" | "customAlias">) {
  return d.customAlias || d.alias;
}

// ── Actions ───────────────────────────────────────────────────────────────

export async function respond(request: IncomingRequest, decision: Partial<Decision>) {
  store.requests = store.requests.filter((r) => r.id !== request.id);
  await attempt(() =>
    platform.respond(request.id, { accept: null, decline: false, trust: false, saveDir: null, ...decision }),
  );
}

export async function loadFiles(transferId: string) {
  const files = await attempt(() => platform.transferFiles(transferId));
  if (files) store.files.set(transferId, files);
}

export async function saveSettings(patch: Partial<Settings>) {
  if (!store.settings) return;
  const next = { ...store.settings, ...patch };
  const saved = await attempt(() => platform.updateSettings(next));
  if (saved) store.settings = saved;
  return saved;
}

export async function setFlags(id: string, flags: Parameters<typeof platform.setDeviceFlags>[1]) {
  const d = await attempt(() => platform.setDeviceFlags(id, flags));
  if (d) store.devices.set(d.id, d);
}
