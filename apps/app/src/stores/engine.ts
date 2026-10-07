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
  Snapshot,
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
  /** Loading (or reloading) the engine's state failed; Retry calls `initEngine`. */
  syncError: null as string | null,
  local: null as LocalDevice | null,
  devices: new Map<string, DeviceSummary>(),
  transfers: new Map<string, TransferSummary>(),
  /** Per-file details, kept only for transfers the user expanded. */
  files: new Map<string, TransferFile[]>(),
  requests: [] as IncomingRequest[],
  /** Incoming messages, newest first (at most MAX_MESSAGES). */
  messages: [] as IncomingRequest[],
  history: [] as HistoryEntry[],
  /** Browser: newest Inbox entries (received files are listed even while history is off). */
  inbox: [] as HistoryEntry[],
  /** Bumped after history or received files were cleared, so lists reload. */
  historyRevision: 0,
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

/** Unread messages kept for the message card; older ones are dropped. */
const MAX_MESSAGES = 20;
const MAX_HISTORY = 400;

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
      if (event.request.text != null) {
        if (!store.messages.some((m) => m.id === event.request.id)) {
          store.messages.unshift(event.request);
          if (store.messages.length > MAX_MESSAGES) store.messages.length = MAX_MESSAGES;
        }
      } else if (!answered.has(event.request.id)) {
        upsert(store.requests, event.request);
      }
      break;
    case "incomingRequestClosed":
      answered.delete(event.id);
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
      if (store.history.some((h) => h.id === event.entry.id)) break;
      store.history.unshift(event.entry);
      if (store.history.length > MAX_HISTORY) store.history.length = MAX_HISTORY;
      break;
    case "inboxAdded":
      if (store.inbox.some((h) => h.id === event.entry.id)) break;
      store.inbox.unshift(event.entry);
      if (store.inbox.length > MAX_HISTORY) store.inbox.length = MAX_HISTORY;
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
      if (!answered.has(event.request.id)) upsert(store.pairing.requests, event.request);
      break;
    case "pairingRequestClosed":
      answered.delete(event.id);
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

function upsert<T extends { id: string }>(list: T[], item: T) {
  const i = list.findIndex((x) => x.id === item.id);
  if (i >= 0) list[i] = item;
  else list.push(item);
}

function replaceAll<T extends { id: string }>(map: Map<string, T>, items: T[]) {
  map.clear();
  for (const item of items) map.set(item.id, item);
}

// ── Startup and resync ────────────────────────────────────────────────────
//
// Listeners are installed (and awaited) before the snapshot is requested, so
// no event falls between the two. Events that arrive while a snapshot loads
// are queued and applied after it: the snapshot may be older than they are,
// never newer than the last event about the same thing. Every handler is an
// idempotent upsert or removal by id, so replaying an event the snapshot
// already reflects changes nothing.

let unsubscribe: (() => void) | null = null;
/** Events received while a snapshot loads; null when none is loading. */
let queue: EngineEvent[] | null = null;
let syncing: Promise<boolean> | null = null;
let syncAgain = false;
let syncToast: number | null = null;
/**
 * Prompts answered here that the engine hasn't closed yet: a snapshot read in
 * between must not bring them back.
 */
const answered = new Set<string>();

export function markAnswered(id: string) {
  answered.add(id);
}

function onEvent(event: EngineEvent) {
  if (queue) queue.push(event);
  else handle(event);
}

/**
 * Subscribes (once) and loads the engine's state. Also the Retry action after
 * a failure. Resolves to whether the state is loaded.
 */
export async function initEngine(): Promise<boolean> {
  if (!unsubscribe) {
    try {
      unsubscribe = await platform.subscribe(onEvent, () => void resync());
    } catch (err) {
      syncFailed(err);
      return false;
    }
  }
  return resync();
}

/** Stops listening to the engine (tests, hot reload). */
export function stopEngine() {
  unsubscribe?.();
  unsubscribe = null;
}

/**
 * Replaces the mirrored state with a fresh snapshot (on startup, and when the
 * shell reports missed events). Requests during a load run one more load.
 */
export function resync(): Promise<boolean> {
  if (syncing) {
    syncAgain = true;
    return syncing;
  }
  syncing = (async () => {
    let ok: boolean;
    do {
      syncAgain = false;
      ok = await loadSnapshot();
    } while (syncAgain);
    return ok;
  })().finally(() => (syncing = null));
  return syncing;
}

async function loadSnapshot(): Promise<boolean> {
  queue = [];
  try {
    const snap = await platform.init();
    // Engines without these in their snapshot answer separately; a failed
    // lookup keeps what is shown.
    const [history, inbox, links, rooms, signaling] = await Promise.all([
      platform.history(120).catch(() => null),
      platform.inbox ? platform.inbox(120).catch(() => null) : null,
      snap.browserLinks ?? platform.browserLinks().catch(() => null),
      snap.rooms ?? platform.rooms().catch(() => null),
      snap.signaling !== undefined ? snap.signaling : platform.signalingStatus().catch(() => undefined),
    ]);
    applySnapshot(snap);
    if (history) store.history = history;
    if (inbox) store.inbox = inbox;
    if (links) replaceAll(store.links, links);
    if (rooms) replaceAll(store.rooms, rooms);
    if (signaling !== undefined) store.signaling = signaling;
    store.syncError = null;
    if (syncToast != null) dismissToast(syncToast);
    syncToast = null;
    store.ready = true;
    return true;
  } catch (err) {
    syncFailed(err);
    return false;
  } finally {
    const pending = queue ?? [];
    queue = null;
    for (const event of pending) handle(event);
  }
}

function applySnapshot(snap: Snapshot) {
  store.local = snap.local;
  store.settings = snap.settings;
  store.server = { ...snap.server };
  replaceAll(store.devices, snap.devices);
  const before = new Map(store.transfers);
  replaceAll(store.transfers, snap.transfers);
  for (const id of [...store.files.keys()]) if (!store.transfers.has(id)) store.files.delete(id);
  // Finished while the events were missed: same notice as live.
  for (const t of snap.transfers) {
    const prev = before.get(t.id);
    if (prev && !isFinal(prev.state) && isFinal(t.state)) onFinished(t);
  }
  if (snap.pendingRequests) {
    const open = new Set(snap.pendingRequests.map((r) => r.id));
    store.requests = snap.pendingRequests.filter((r) => !answered.has(r.id));
    if (snap.pairingRequests) {
      for (const r of snap.pairingRequests) open.add(r.id);
      store.pairing.requests = snap.pairingRequests.filter((r) => !answered.has(r.id));
    }
    // Answered prompts the engine has since closed.
    for (const id of [...answered]) if (!open.has(id)) answered.delete(id);
  }
  if (store.pairing.offer && store.pairing.offer.expiresAtMs <= Date.now()) store.pairing.offer = null;
}

function syncFailed(err: unknown) {
  const { title } = errorText(err);
  store.syncError = title;
  if (syncToast != null) dismissToast(syncToast);
  syncToast = toast(
    {
      level: "error",
      title: store.ready ? "Ferry may be showing out-of-date information" : "Ferry couldn't start",
      body: title,
      action: { label: "Retry", run: () => void initEngine() },
    },
    0,
  );
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

/** What arrived, newest first: the Inbox's own list in the browser, received history elsewhere. */
export const received = computed(() => (platform.inbox ? store.inbox : store.history.filter((h) => h.direction === "receive")));

export function displayName(d: Pick<DeviceSummary, "alias" | "customAlias">) {
  return d.customAlias || d.alias;
}

// ── Actions ───────────────────────────────────────────────────────────────

export async function respond(request: IncomingRequest, decision: Partial<Decision>) {
  markAnswered(request.id);
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
