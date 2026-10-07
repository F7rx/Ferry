// Browser (PWA) platform: devices are found through a ferry-signal server and
// files stream peer to peer over WebRTC (docs/05-protocol.md §5). The server
// only relays connection setup; it never sees file contents. Received files
// stay in this browser's private storage until saved (see web-sink.ts).
import {
  deserializeFromIdb,
  exportPublicKey,
  fileNameProblem,
  generateIdentity,
  PeerConnector,
  randomBytes,
  randomId,
  relayedOf,
  roomIdFromSecret,
  RtcError,
  b64urlEncode,
  serializeForIdb,
  SignalingClient,
  type ClientInfo,
  type FileMeta,
  type Identity,
  type IncomingOffer,
  type PeerSession,
  type RemotePeer,
  type SinkContext,
  type SourceFile,
  type TransferRequest,
  type WebSocketFactory,
} from "../lib/rtc";
import { tryB64urlDecode } from "../lib/rtc/bytes";
import { store } from "../lib/idb";
import { itemsFromFileList } from "../lib/dropfiles";
import { createBrowserSink, deleteStored, hasOpfs, idOfPath, pruneStored, readStored, storageId, storedSize } from "./web-sink";
import type {
  Capabilities,
  ConnectionInfo,
  Decision,
  DeviceKind,
  DeviceSummary,
  DiagnosticCheck,
  Direction,
  EngineEvent,
  HistoryEntry,
  IncomingRequest,
  LocalDevice,
  OutgoingItem,
  PeerRef,
  Platform,
  RoomInfo,
  SignalingStatus,
  SendTarget,
  Settings,
  Snapshot,
  TransferFile,
  TransferState,
  TransferSummary,
} from "./types";

interface KnownDevice {
  /** Identity public key (base64url): the device's id. */
  id: string;
  alias: string;
  deviceKind: DeviceKind;
  deviceModel: string | null;
  customAlias: string | null;
  trusted: boolean;
  favorite: boolean;
  lastSeenMs: number;
}

interface Tracked {
  /** `summary.id` is this device's own random id for the transfer, never the peer's. */
  summary: TransferSummary;
  /** The transfer id on the wire (chosen by the sender): used in cancel/resume messages and resume records. */
  wireId: string;
  files: TransferFile[];
  /** The session driving it now; events from any other session are ignored. */
  session: PeerSession | null;
  /** Bytes per file id, for progress. */
  done: Map<string, number>;
  /** Recent (time, bytes) points; speed is measured over this window. */
  samples: { at: number; bytes: number }[];
  /** The other device's identity key. */
  key: string;
  /** Sender: what to offer again after an interruption (Files stay valid while the tab is open). */
  request?: TransferRequest;
  retrying?: boolean;
  attempts?: number;
  /** Sender: file ids already written to history, so a transfer that is tried again never lists a file twice. */
  recorded?: Set<string>;
  userCancelled?: boolean;
  /** Receiver: gives up waiting for the sender to come back. */
  giveUp?: ReturnType<typeof setTimeout>;
}

/** Receiver side of a transfer it accepted, so a re-offer after an interruption resumes without asking again. */
interface ResumeRecord {
  key: string;
  transferId: string;
  /** Accepted files; a re-offer must match id, name, size and type exactly. */
  files: Record<string, { name: string; size: number; mime: string }>;
  /** Chosen by this receiver and mixed into storage ids, so a peer can't aim a new transfer at stored files. */
  nonce: string;
  at: number;
}

/**
 * A history record as stored. `inbox`: a received file the Inbox lists (kept
 * whatever the history settings, so the file stays reachable). `activity`: an
 * entry the History page shows (only kept while history is on). Version 1
 * records carry neither flag; see `isInboxFile` / `isActivity`.
 */
interface StoredEntry extends HistoryEntry {
  inbox?: boolean;
  activity?: boolean;
}

/** A private link room: devices that opened the same link see each other anywhere. */
interface Room {
  id: string;
  secret: Uint8Array;
  link: string;
  createdAtMs: number;
  /** Identity keys of the other devices in it now. */
  peers: Set<string>;
}

interface Pending {
  request: IncomingRequest;
  offer: IncomingOffer;
  session: PeerSession;
}

const kv = store<unknown>("kv");
const knownDb = store<KnownDevice>("devices");
const historyDb = store<StoredEntry>("history");

const isInboxFile = (e: StoredEntry) => e.inbox ?? (e.direction === "receive" && e.kind === "file" && !!e.path);
const isActivity = (e: StoredEntry) => e.activity ?? true;
/** What the Inbox lists: received files, and received messages while they are kept in history. */
const inInbox = (e: StoredEntry) => e.direction === "receive" && (isInboxFile(e) || (e.kind === "text" && isActivity(e)));
function plain(e: StoredEntry): HistoryEntry {
  const { inbox: _inbox, activity: _activity, ...entry } = e;
  return entry;
}

const fail = (code: string, message: string, hint?: string) => ({ code, message, hint });
const unsupported = (what: string) => fail("unsupported", `${what} isn't available in the browser.`, "Install the Ferry app for this.");
const now = () => Date.now();

const DEFAULT_STUN = ["stun:stun.l.google.com:19302"];
/** How long an interrupted transfer can be resumed. */
const RESUME_TTL_MS = 24 * 3600_000;
/** Sender reconnect attempts after an interruption (then "Try again"). */
const RETRY_DELAYS_MS = [1000, 2000, 4000, 8000, 15_000, 30_000];
/** Receiver waits this long for the sender to come back. */
const RECEIVER_WAIT_MS = 3 * 60_000;
/** Failures that end a transfer; anything else (closed, webrtc, timeout, offline) is retried. */
const FATAL_CODES = new Set(["invalid", "invalid-state", "auth", "protocol", "rejected", "too-large", "source", "no-sink", "internal", "overrun", "bad_name", "unsupported"]);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
/** Inbound connections per peer key, and undecided offers per peer / overall (the native engine caps these too). */
const MAX_INBOUND_PER_PEER = 2;
const MAX_PENDING_PER_PEER = 3;
const MAX_PENDING = 16;
/** Messages admitted per peer / overall within MESSAGE_WINDOW_MS; more are declined. */
const MAX_MESSAGES_PER_PEER = 10;
const MAX_MESSAGES = 30;
const MESSAGE_WINDOW_MS = 60_000;
/** Messages remembered (peer, transfer id) so a re-sent transfer doesn't show its text twice. */
const MAX_DELIVERED = 256;
/** Finished transfers kept in memory; older ones are dismissed. */
const MAX_FINISHED = 50;
/** Startup cleanup leaves files written this recently alone (another tab may be receiving them). */
const PRUNE_GRACE_MS = 15 * 60_000;
const PAGE = 500;
/** Types a received file may be opened as in a tab; anything else is saved instead (never HTML/SVG/XML on our origin). */
const INLINE_SAFE = /^(image\/(png|jpeg|gif|webp|avif)|video\/(mp4|webm|ogg)|audio\/(mpeg|mp4|ogg|wav|webm|flac)|application\/pdf|text\/plain)$/;
const RASTER = /^image\/(png|jpeg|gif|webp|avif)$/;

function browserName(): string {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua)
    ? "Edge"
    : /OPR\//.test(ua)
      ? "Opera"
      : /Firefox\//.test(ua)
        ? "Firefox"
        : /Chrome\//.test(ua)
          ? "Chrome"
          : /Safari\//.test(ua)
            ? "Safari"
            : "Browser";
  const os = /Android/.test(ua)
    ? "Android"
    : /iPhone|iPad|iPod/.test(ua)
      ? "iOS"
      : /Windows/.test(ua)
        ? "Windows"
        : /Mac OS X/.test(ua)
          ? "macOS"
          : /Linux|CrOS/.test(ua)
            ? "Linux"
            : "";
  return os ? `${browser} on ${os}` : browser;
}

function defaultSettings(): Settings {
  return {
    version: 1,
    alias: browserName(),
    deviceKind: "web",
    deviceModel: null,
    receiveEnabled: true,
    saveDir: null,
    autoAccept: "trusted",
    pin: null,
    decisionTimeoutSecs: 300,
    historyEnabled: true,
    keepMessageText: false,
    checksumsForLocalsend: false,
    verifyIncomingChecksums: true,
    parallelFiles: 1,
    port: 0,
    encryption: true,
    multicastGroup: "",
    ipv6: false,
    includeVirtualInterfaces: false,
    interfaceWhitelist: null,
    interfaceBlacklist: null,
    subnetScan: false,
    signalingUrl: null,
    stunServers: DEFAULT_STUN,
  };
}

/** Same-origin `/v1/ws` unless the build or the settings name another server. */
function signalingUrl(settings: Settings): string {
  if (settings.signalingUrl) return settings.signalingUrl;
  const built = import.meta.env.VITE_FERRY_SIGNAL_URL as string | undefined;
  if (built) return built;
  return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/v1/ws`;
}

function kindOf(type: string | undefined): DeviceKind {
  switch ((type ?? "").toLowerCase()) {
    case "mobile":
      return "mobile";
    case "desktop":
      return "desktop";
    case "headless":
      return "headless";
    case "server":
      return "server";
    default:
      return "web";
  }
}

function isFinal(state: TransferState) {
  return ["completed", "completedWithErrors", "declined", "cancelled", "failed"].includes(state);
}

/** Hex groups of SHA-256(public key), shown next to the name like a fingerprint. */
async function shortIdOf(publicKey: string): Promise<string> {
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(publicKey)));
  const hex = [...hash.slice(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join("").toUpperCase();
  return hex.match(/.{4}/g)!.join(" ");
}

function source(id: string, name: string, file: File): SourceFile {
  return {
    id,
    name,
    size: file.size,
    mime: file.type || "application/octet-stream",
    modified: file.lastModified || undefined,
    slice: (start, end) => file.slice(start, end).arrayBuffer(),
  };
}

function pickInput(options: { multiple?: boolean; directory?: boolean }): Promise<File[] | null> {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.multiple = !!options.multiple;
    if (options.directory) input.webkitdirectory = true;
    input.style.display = "none";
    const done = (files: File[] | null) => {
      input.remove();
      resolve(files);
    };
    input.addEventListener("change", () => done(input.files?.length ? [...input.files] : null), { once: true });
    input.addEventListener("cancel", () => done(null), { once: true });
    document.body.append(input);
    input.click();
  });
}

/** Test seams: a fake signaling server and RTCPeerConnection. */
export interface WebPlatformOptions {
  WebSocket?: WebSocketFactory;
  RTCPeerConnection?: typeof RTCPeerConnection;
  /** Delays between automatic reconnect attempts of a send (default RETRY_DELAYS_MS). */
  retryDelaysMs?: readonly number[];
}

/** The first line of a message, short enough for a list. */
function firstLine(text: string): string {
  return text.trim().split("\n")[0]!.trim().slice(0, 80) || "Message";
}

export function createWebPlatform(options: WebPlatformOptions = {}): Platform & {
  takeShared(): Promise<OutgoingItem[]>;
  inbox(limit: number, beforeId?: number): Promise<HistoryEntry[]>;
  clearReceivedFiles(): Promise<{ deleted: number; failed: number }>;
} {
  const capabilities: Capabilities = {
    kind: "web",
    lanDiscovery: false,
    receiveInBackground: false,
    pickFolders: true,
    revealInFolder: false,
    clipboardRead: typeof navigator !== "undefined" && !!navigator.clipboard?.readText,
    nativeNotifications: typeof window !== "undefined" && "Notification" in window,
    tray: false,
    localsendInterop: false,
    browserLinks: false,
    pairing: false,
    remoteLinks: true,
  };

  const handlers = new Set<(e: EngineEvent) => void>();
  const emit = (e: EngineEvent) => handlers.forEach((h) => h(e));

  let identity: Identity;
  let publicKey = "";
  let shortId = "";
  let settings = defaultSettings();
  let signaling: SignalingClient | null = null;
  let connector: PeerConnector | null = null;
  let signalingState: "connecting" | "open" | "closed" = "closed";

  const known = new Map<string, KnownDevice>();
  /** Devices present on the signaling server right now, by identity key. */
  const present = new Map<string, ClientInfo>();
  /** How they are present: in our nearby group and/or in one of our rooms. */
  const nearby = new Set<string>();
  const rooms = new Map<string, Room>();
  const sessions = new Map<string, PeerSession>();
  const connecting = new Map<string, Promise<PeerSession>>();
  /** Every live transfer by its own (UI) id. */
  const transfers = new Map<string, Tracked>();
  /** The same transfers by (authenticated peer key, direction, wire id): what session events are matched on. */
  const byWire = new Map<string, Tracked>();
  const pending = new Map<string, Pending>();
  /** Object URLs for received images (previews), by history path. */
  const previews = new Map<string, string>();
  let resumeRecords: ResumeRecord[] = [];
  /** Storage nonce per accepted transfer ("<peer key> <transfer id>"). */
  const nonces = new Map<string, string>();
  const inbound = new Map<string, number>();
  /** The RTCPeerConnection under each session, and whether it is relayed (from its stats). */
  const connections = new WeakMap<PeerSession, RTCPeerConnection | null>();
  const routes = new WeakMap<PeerSession, Promise<boolean | null>>();
  /** When messages were admitted, per peer key. */
  const messageTimes = new Map<string, number[]>();
  /** "<peer key> <transfer id>" of messages already shown, oldest first. */
  const delivered = new Set<string>();

  // ── Devices ─────────────────────────────────────────────────────────────

  function summary(key: string): DeviceSummary | null {
    const live = present.get(key);
    const k = known.get(key);
    if (!live && !k) return null;
    return {
      id: key,
      alias: live?.alias ?? k!.alias,
      deviceModel: live?.deviceModel ?? k?.deviceModel ?? null,
      deviceKind: live ? kindOf(live.deviceType) : (k?.deviceKind ?? "web"),
      // Every session pins this key; the peer proves it in the handshake.
      verified: true,
      protocol: "https",
      isFerry: true,
      trusted: !!k?.trusted,
      favorite: !!k?.favorite,
      mine: false,
      online: !!live,
      lastSeenMs: live ? now() : (k?.lastSeenMs ?? 0),
      address: live && !nearby.has(key) && roomOf(key) ? "via private link" : null,
      ipVersion: null,
      rttMs: null,
      customAlias: k?.customAlias ?? null,
      download: false,
    };
  }

  function announce(key: string) {
    const d = summary(key);
    if (d) emit({ type: "deviceUpdated", device: d });
    else emit({ type: "deviceRemoved", id: key });
  }

  function keyOf(peer: ClientInfo): string | null {
    // Only Ferry clients (with an identity key) can talk ferry-dc/1.
    return peer.ext?.key ?? null;
  }

  function roomOf(key: string): Room | undefined {
    for (const r of rooms.values()) if (r.peers.has(key)) return r;
    return undefined;
  }

  function roomInfo(r: Room): RoomInfo {
    return { id: r.id, link: r.link, peers: r.peers.size, createdAtMs: r.createdAtMs };
  }

  function seen(peer: ClientInfo, room?: Room) {
    const key = keyOf(peer);
    if (!key || key === publicKey) return;
    if (room) room.peers.add(key);
    else nearby.add(key);
    present.set(key, peer);
    const k = known.get(key);
    if (k) k.lastSeenMs = now();
    announce(key);
    if (room) emit({ type: "roomUpdated", room: roomInfo(room) });
  }

  /** Forgets `key` unless it is still reachable another way. */
  function drop(key: string) {
    if (!nearby.has(key) && !roomOf(key)) present.delete(key);
    announce(key);
  }

  function clearPresence() {
    nearby.clear();
    for (const r of rooms.values()) {
      r.peers.clear();
      emit({ type: "roomUpdated", room: roomInfo(r) });
    }
    for (const key of [...present.keys()]) drop(key);
  }

  function gone(clientId: string, room?: Room) {
    for (const [key, peer] of [...present]) {
      if (peer.id !== clientId) continue;
      if (room) {
        room.peers.delete(key);
        emit({ type: "roomUpdated", room: roomInfo(room) });
      } else {
        nearby.delete(key);
      }
      drop(key);
    }
  }

  function joinSecret(secret: Uint8Array): RoomInfo {
    const id = roomIdFromSecret(secret);
    let room = rooms.get(id);
    if (!room) {
      room = { id, secret, link: `${location.origin}/#room=${b64urlEncode(secret)}`, createdAtMs: now(), peers: new Set() };
      rooms.set(id, room);
      signaling?.joinRoom(id);
    }
    const info = roomInfo(room);
    emit({ type: "roomUpdated", room: info });
    return info;
  }

  /** Accepts a whole link (`…#room=<secret>`) or just the secret. */
  function joinLink(input: string): RoomInfo {
    const match = /(?:^|[#&?])room=([A-Za-z0-9_-]+)/.exec(input.trim());
    const secret = tryB64urlDecode(match ? match[1]! : input.trim());
    if (!secret || secret.length < 16 || secret.length > 64) throw fail("bad_link", "That isn't a Ferry link.");
    return joinSecret(secret);
  }

  function leave(id: string): boolean {
    const room = rooms.get(id);
    if (!room) return false;
    rooms.delete(id);
    signaling?.leaveRoom(id);
    for (const key of room.peers) drop(key);
    emit({ type: "roomRemoved", id });
    return true;
  }

  function peerRef(key: string, remote?: RemotePeer | null): PeerRef {
    const d = summary(key);
    return {
      id: key,
      alias: d?.customAlias ?? d?.alias ?? remote?.device.alias ?? "Browser",
      deviceKind: d?.deviceKind ?? kindOf(remote?.device.deviceType),
      deviceModel: d?.deviceModel ?? null,
      verified: true,
    };
  }

  async function remember(key: string, patch: Partial<KnownDevice>) {
    const base: KnownDevice = known.get(key) ?? {
      id: key,
      alias: present.get(key)?.alias ?? "Browser",
      deviceKind: kindOf(present.get(key)?.deviceType),
      deviceModel: present.get(key)?.deviceModel ?? null,
      customAlias: null,
      trusted: false,
      favorite: false,
      lastSeenMs: now(),
    };
    const next = { ...base, ...patch };
    if (!next.trusted && !next.favorite && !next.customAlias) {
      known.delete(key);
      await knownDb.delete(key);
    } else {
      known.set(key, next);
      await knownDb.put(next);
    }
    announce(key);
  }

  // ── Signaling and sessions ──────────────────────────────────────────────

  function serverStatus() {
    const url = signalingUrl(settings);
    return {
      running: signalingState === "open",
      port: 0,
      error: signalingState === "closed" ? `Can't reach the signaling server (${url}). Retrying…` : null,
    };
  }

  function currentSignaling(): SignalingStatus {
    const status = serverStatus();
    return { url: signalingUrl(settings), state: signalingState, error: status.error, identityKey: publicKey };
  }

  /** Tells the UI about the signaling connection (both the legacy server status and the typed status). */
  function emitSignaling() {
    emit({ type: "serverStatus", ...serverStatus() });
    emit({ type: "signalingStatus", status: currentSignaling() });
  }

  function iceServers(): () => Promise<RTCIceServer[]> {
    return async () => {
      const servers: RTCIceServer[] = settings.stunServers.length ? [{ urls: settings.stunServers }] : [];
      if (signaling?.server?.caps.includes("turn")) {
        const turn = await signaling.fetchTurn().catch(() => null);
        if (turn) servers.push(...turn.iceServers);
      }
      return servers;
    };
  }

  function startSignaling() {
    connector?.dispose();
    // Detach before closing, so the old client's last "closed" isn't reported as the new one's.
    const previous = signaling;
    signaling = null;
    previous?.close();
    present.clear();
    nearby.clear();
    sessions.clear();
    connecting.clear();
    const client = new SignalingClient({
      url: signalingUrl(settings),
      info: { alias: settings.alias, deviceType: "web", deviceModel: browserName(), token: randomId(), publicKey },
      ...(options.WebSocket ? { WebSocket: options.WebSocket } : {}),
    });
    signaling = client;
    // A replaced client may still report (its close, a late message): only the current one counts.
    const live = () => signaling === client;
    // `connect()` below reports "connecting" right away.
    signalingState = client.state;
    client.on("state", (state) => {
      if (!live()) return;
      signalingState = state;
      emitSignaling();
      if (state !== "open") clearPresence();
    });
    client.on("hello", ({ peers }) => {
      if (!live()) return;
      for (const key of [...nearby]) {
        nearby.delete(key);
        drop(key);
      }
      peers.forEach((p) => seen(p));
    });
    client.on("join", ({ peer }) => live() && seen(peer));
    client.on("update", ({ peer }) => {
      if (!live()) return;
      const key = keyOf(peer);
      const room = key && !nearby.has(key) ? roomOf(key) : undefined;
      seen(peer, room);
    });
    client.on("left", ({ peerId }) => live() && gone(peerId));
    client.on("roomHello", ({ room: id, peers }) => {
      const room = rooms.get(id);
      if (!room || !live()) return;
      for (const key of [...room.peers]) {
        room.peers.delete(key);
        drop(key);
      }
      peers.forEach((p) => seen(p, room));
      emit({ type: "roomUpdated", room: roomInfo(room) });
    });
    client.on("roomPeerJoined", ({ room: id, peer }) => {
      const room = rooms.get(id);
      if (room && live()) seen(peer, room);
    });
    client.on("roomPeerLeft", ({ room: id, peerId }) => {
      const room = rooms.get(id);
      if (room && live()) gone(peerId, room);
    });
    for (const room of rooms.values()) {
      room.peers.clear();
      client.joinRoom(room.id);
    }

    const sink = createBrowserSink(onCommitted, (context) => {
      const nonce = nonces.get(`${context.peerKey} ${context.transferId}`);
      // Every accepted transfer has one; without it the storage id would be guessable.
      if (!nonce) throw new RtcError("invalid-state", "This transfer wasn't accepted here");
      return nonce;
    });
    const current = new PeerConnector({
      signaling: client,
      identity,
      device: { alias: settings.alias, deviceType: "web", platform: "browser" },
      sink,
      iceServers: iceServers(),
      session: { decisionTimeoutMs: settings.decisionTimeoutSecs * 1000 },
      ...(options.RTCPeerConnection ? { RTCPeerConnection: options.RTCPeerConnection } : {}),
    });
    connector = current;
    current.on("incoming", (request) => {
      const key = keyOf(request.peer);
      if (!key || !settings.receiveEnabled || connector !== current) return request.reject();
      if ((inbound.get(key) ?? 0) >= MAX_INBOUND_PER_PEER) return request.reject();
      inbound.set(key, (inbound.get(key) ?? 0) + 1);
      const release = () => inbound.set(key, Math.max(0, (inbound.get(key) ?? 1) - 1));
      // The connection itself carries no files; every offer is decided separately.
      // Peers met through a link must also prove they know its secret.
      request
        .accept({ expectedPeerKey: key, roomSecret: roomOf(key)?.secret })
        .then((session) => session.on("closed", release))
        .catch(release);
    });
    current.on("session", ({ session, connection }) => {
      connections.set(session, connection);
      wire(session);
    });
    client.connect();
  }

  /** The transfer a session event is about: same authenticated peer, direction and wire id, and driven by this very session. */
  function trackedFor(session: PeerSession, direction: Direction, wireId: string): Tracked | undefined {
    const key = session.peer?.key;
    if (!key) return undefined;
    const t = byWire.get(wireKey(key, direction, wireId));
    return t && t.session === session ? t : undefined;
  }

  function wire(session: PeerSession) {
    session.ready
      .then((peer) => {
        const previous = sessions.get(peer.key);
        if (!previous || previous.state === "closed") sessions.set(peer.key, session);
        // Names are only remembered from a peer that proved its key.
        const k = known.get(peer.key);
        if (k && k.alias !== peer.device.alias) {
          k.alias = peer.device.alias.slice(0, 64);
          void knownDb.put({ ...k });
          announce(peer.key);
        }
      })
      .catch(() => {});
    session.on("offer", (offer) => void onOffer(session, offer));
    session.on("accepted", (a) => {
      const t = trackedFor(session, "send", a.transferId);
      if (!t || isFinal(t.summary.state)) return;
      const accepted = new Set(a.files);
      t.files = t.files.map((f) => (accepted.has(f.id) ? { ...f, state: "transferring" } : { ...f, state: "skipped" }));
      t.summary.fileCount = a.files.length;
      t.samples = [{ at: now(), bytes: 0 }];
      t.summary.totalBytes = t.files.filter((f) => accepted.has(f.id)).reduce((n, f) => n + f.size, 0);
      update(t, { state: "transferring" });
    });
    session.on("progress", (p) => {
      const t = trackedFor(session, p.direction, p.transferId);
      if (!t || isFinal(t.summary.state)) return;
      t.done.set(p.fileId, p.bytes);
      const f = t.files.find((x) => x.id === p.fileId);
      if (f) f.bytesDone = p.bytes;
      const bytes = [...t.done.values()].reduce((a, b) => a + b, 0);
      const at = now();
      t.samples.push({ at, bytes });
      while (t.samples.length > 2 && at - t.samples[0]!.at > 3000) t.samples.shift();
      const first = t.samples[0]!;
      const span = (at - first.at) / 1000;
      const speed = span >= 0.4 ? (bytes - first.bytes) / span : t.summary.speedBps;
      const left = t.summary.totalBytes - bytes;
      update(t, { bytesDone: bytes, speedBps: Math.max(0, Math.round(speed)), etaSecs: speed > 0 ? Math.ceil(left / speed) : null });
    });
    session.on("fileComplete", (c) => {
      const t = trackedFor(session, c.direction, c.transferId);
      if (!t || isFinal(t.summary.state)) return;
      const f = t.files.find((x) => x.id === c.fileId);
      if (f) {
        f.state = c.ok ? "done" : "failed";
        if (c.ok) f.bytesDone = f.size;
        if (!c.ok) f.error = fail("file_failed", c.error ?? "The file didn't arrive intact.");
      }
      update(t, { filesDone: t.files.filter((x) => x.state === "done").length });
      emit({ type: "transferFilesUpdated", id: t.summary.id, files: t.files.map((x) => ({ ...x })) });
    });
    session.on("done", (d) => {
      const t = trackedFor(session, d.direction, d.transferId);
      if (!t) return;
      const state: TransferState = d.failed.length ? (d.completed.length ? "completedWithErrors" : "failed") : "completed";
      if (t.summary.direction === "receive") void forgetResume(t.key, t.wireId);
      finish(t, state, d.failed.length ? fail("files_failed", `${d.failed.length} file(s) didn't arrive intact.`) : null);
    });
    session.on("cancelled", (c) => {
      if (c.direction === "receive") {
        const request = [...pending.values()].find((p) => p.offer.transferId === c.transferId && p.session === session);
        if (request) {
          pending.delete(request.request.id);
          emit({ type: "incomingRequestClosed", id: request.request.id, reason: c.reason ?? "cancelled" });
        }
      }
      const t = trackedFor(session, c.direction, c.transferId);
      if (!t || isFinal(t.summary.state)) return;
      if (c.interrupted && !t.userCancelled) interrupted(t);
      else {
        if (t.summary.direction === "receive") void forgetResume(t.key, t.wireId);
        finish(t, "cancelled", c.byRemote ? fail("cancelled_by_peer", `${t.summary.peer.alias} cancelled.`) : null);
      }
    });
    session.on("closed", () => {
      for (const [key, s] of sessions) if (s === session) sessions.delete(key);
    });
  }

  async function sessionFor(key: string): Promise<PeerSession> {
    const open = sessions.get(key);
    if (open && open.state === "ready") return open;
    const inFlight = connecting.get(key);
    if (inFlight) return inFlight;
    const peer = present.get(key);
    if (!peer || !connector) throw fail("offline", `${summary(key)?.alias ?? "That device"} is offline.`);
    const attempt = connector
      .connect(peer.id, undefined, { expectedPeerKey: key, roomSecret: roomOf(key)?.secret })
      .then(async (session) => {
        await session.ready;
        sessions.set(key, session);
        return session;
      })
      .finally(() => connecting.delete(key));
    connecting.set(key, attempt);
    return attempt;
  }

  // ── Transfers ───────────────────────────────────────────────────────────

  /** Relayed is only known once the browser reports the selected candidate pair. */
  const webrtc: ConnectionInfo = { transport: "webrtc", encrypted: true, ipVersion: null, relayed: null, address: null };

  function wireKey(key: string, direction: Direction, wireId: string) {
    return `${key}\n${direction}\n${wireId}`;
  }

  function track(direction: Direction, peer: PeerRef, files: readonly FileMeta[], text: string | null, dropId: string | null, wireId: string, session: PeerSession | null, key: string): Tracked {
    const first = files[0];
    const summaryValue: TransferSummary = {
      id: randomId(),
      direction,
      dropId,
      peer,
      state: "preparing",
      fileCount: files.length,
      filesDone: 0,
      totalBytes: files.reduce((n, f) => n + f.size, 0),
      bytesDone: 0,
      speedBps: 0,
      etaSecs: null,
      startedAtMs: now(),
      finishedAtMs: null,
      connection: { ...webrtc },
      resumable: true,
      // Browser transfers continue after a lost connection, but can't be paused.
      canPause: false,
      canResume: false,
      title: first ? (files.length > 1 ? `${first.name.split("/")[0]} and ${files.length - 1} more` : first.name) : "Message",
      text,
      error: null,
      saveDir: null,
    };
    const t: Tracked = {
      summary: summaryValue,
      wireId,
      files: files.map((f) => ({ id: f.id, name: f.name, size: f.size, mime: f.mime, state: "pending", bytesDone: 0 })),
      session,
      done: new Map(),
      samples: [{ at: now(), bytes: 0 }],
      key,
    };
    // An older transfer with the same peer, direction and wire id gives way.
    const older = byWire.get(wireKey(key, direction, wireId));
    if (older) {
      if (!isFinal(older.summary.state)) {
        older.session = null;
        finish(older, "failed", fail("superseded", `${peer.alias} started this transfer again.`));
      }
      remove(older);
    }
    transfers.set(summaryValue.id, t);
    byWire.set(wireKey(key, direction, wireId), t);
    emit({ type: "transferUpdated", transfer: { ...summaryValue } });
    if (session) void routeOf(t, session);
    return t;
  }

  function remove(t: Tracked) {
    if (t.giveUp) clearTimeout(t.giveUp);
    transfers.delete(t.summary.id);
    const k = wireKey(t.key, t.summary.direction, t.wireId);
    if (byWire.get(k) === t) byWire.delete(k);
    emit({ type: "transferRemoved", id: t.summary.id });
  }

  /** Keeps at most MAX_FINISHED finished transfers in memory: the oldest are dismissed. */
  function retain() {
    const finished = [...transfers.values()].filter((t) => isFinal(t.summary.state));
    if (finished.length <= MAX_FINISHED) return;
    finished.sort((a, b) => (a.summary.finishedAtMs ?? 0) - (b.summary.finishedAtMs ?? 0));
    for (const t of finished.slice(0, finished.length - MAX_FINISHED)) remove(t);
  }

  /** Reads whether `session` runs through a TURN relay and shows it on `t`. */
  async function routeOf(t: Tracked, session: PeerSession) {
    let route = routes.get(session);
    if (!route) {
      route = session.ready.then(() => relayedOf(connections.get(session))).catch(() => null);
      routes.set(session, route);
    }
    const relayed = await route;
    if (t.session !== session || t.summary.connection?.relayed === relayed) return;
    update(t, { connection: { ...webrtc, relayed } });
  }

  /** Points `t` at `session`: events from any earlier session no longer touch it. */
  function attach(t: Tracked, session: PeerSession) {
    t.session = session;
    void routeOf(t, session);
  }

  function update(t: Tracked, patch: Partial<TransferSummary>) {
    Object.assign(t.summary, patch);
    emit({ type: "transferUpdated", transfer: { ...t.summary } });
  }

  function finish(t: Tracked, state: TransferState, error: ReturnType<typeof fail> | null) {
    if (isFinal(t.summary.state)) return;
    if (t.giveUp) clearTimeout(t.giveUp);
    t.retrying = false;
    const bytes = state === "completed" ? t.summary.totalBytes : t.summary.bytesDone;
    update(t, { state, error, finishedAtMs: now(), speedBps: 0, etaSecs: null, bytesDone: bytes });
    if (t.summary.direction === "send" && settings.historyEnabled) {
      const entries: Omit<HistoryEntry, "id" | "timestampMs">[] = [];
      const recorded = (t.recorded ??= new Set());
      for (const f of t.files) {
        if (f.state !== "done" && state !== "completed") continue;
        // Recorded when an earlier attempt ended; "Try again" finished the rest.
        if (recorded.has(f.id)) continue;
        recorded.add(f.id);
        entries.push({
          transferId: t.wireId,
          direction: "send",
          peerId: t.summary.peer.id,
          peerAlias: t.summary.peer.alias,
          peerKind: t.summary.peer.deviceKind,
          kind: "file",
          name: f.name,
          size: f.size,
          mime: f.mime,
          path: null,
          text: null,
          status: "completed",
          verified: true,
        });
      }
      void (async () => {
        for (const e of entries) await addEntry(e, { activity: true });
      })().catch((err) => storageNotice("history", err));
    }
    retain();
  }

  function storageNotice(what: "history" | "inbox" | "cleanup", err: unknown) {
    const reason = (err as { name?: string } | null)?.name === "QuotaExceededError" ? "this browser is out of storage space" : String((err as Error)?.message ?? err);
    const message =
      what === "history"
        ? `Couldn't save to history: ${reason}.`
        : what === "inbox"
          ? `A received file couldn't be added to the Inbox: ${reason}. Ask the sender to send it again.`
          : `Couldn't clean up unused received files: ${reason}. Ferry tries again next time it opens.`;
    emit({ type: "notice", level: what === "cleanup" ? "warning" : "error", code: `storage_${what}`, message });
  }

  /** A received file was verified and written. Throws when it can't be listed (the sink then drops it). */
  async function onCommitted(file: FileMeta, context: SinkContext, path: string): Promise<void> {
    // A resumed transfer re-commits files that were already complete.
    if (await historyDb.getBy("path", path)) return;
    const peer = peerRef(context.peerKey);
    try {
      await addEntry(
        {
          transferId: context.transferId,
          direction: "receive",
          peerId: context.peerKey,
          peerAlias: peer.alias,
          peerKind: peer.deviceKind,
          kind: "file",
          name: file.name,
          size: file.size,
          mime: file.mime,
          path,
          text: null,
          status: "completed",
          verified: true,
        },
        // The Inbox is the only way back to a file received in the browser, so it is always listed.
        { inbox: true, activity: settings.historyEnabled },
      );
    } catch (err) {
      storageNotice("inbox", err);
      throw err;
    }
    if (file.mime.startsWith("image/")) void cachePreview(path, file.name, file.mime);
  }

  /** Stores an entry (ids come from the database, so tabs and bursts never collide) and tells the UI. */
  async function addEntry(entry: Omit<HistoryEntry, "id" | "timestampMs">, flags: { inbox?: boolean; activity?: boolean }): Promise<HistoryEntry | null> {
    const inbox = !!flags.inbox;
    const activity = !!flags.activity;
    if (!inbox && !activity) return null;
    const value = { ...entry, timestampMs: now(), inbox, activity } as Omit<StoredEntry, "id">;
    const id = (await historyDb.put(value as StoredEntry)) as number;
    const stored: StoredEntry = { ...value, id };
    const full = plain(stored);
    if (activity) emit({ type: "historyAdded", entry: full });
    if (inInbox(stored)) emit({ type: "inboxAdded", entry: full });
    return full;
  }

  /** Admits a message from `key` unless it (or everyone together) sent too many lately. */
  function admitMessage(key: string): boolean {
    const at = now();
    let total = 0;
    for (const [k, times] of messageTimes) {
      const recent = times.filter((t) => at - t < MESSAGE_WINDOW_MS);
      if (recent.length) messageTimes.set(k, recent);
      else messageTimes.delete(k);
      total += recent.length;
    }
    const mine = messageTimes.get(key) ?? [];
    if (mine.length >= MAX_MESSAGES_PER_PEER || total >= MAX_MESSAGES) return false;
    messageTimes.set(key, [...mine, at]);
    return true;
  }

  /**
   * Shows a message once per (peer, transfer) and records it as the settings
   * allow: nothing without history, only that it arrived without message text.
   */
  function deliverMessage(key: string, peer: PeerRef, trusted: boolean, wireId: string, text: string) {
    const id = `${key} ${wireId}`;
    if (delivered.has(id)) return;
    delivered.add(id);
    if (delivered.size > MAX_DELIVERED) delivered.delete(delivered.values().next().value!);
    const request: IncomingRequest = {
      id: randomId(),
      peer,
      files: [],
      totalBytes: 0,
      text,
      receivedAtMs: now(),
      trusted,
      defaultSaveDir: "",
      expiresAtMs: now(),
    };
    emit({ type: "incomingRequest", request });
    if (!settings.historyEnabled) return;
    const keep = settings.keepMessageText;
    void addEntry(
      {
        transferId: wireId,
        direction: "receive",
        peerId: key,
        peerAlias: peer.alias,
        peerKind: peer.deviceKind,
        kind: "text",
        name: keep ? firstLine(text) : "Message",
        size: new TextEncoder().encode(text).length,
        mime: "text/plain",
        path: null,
        text: keep ? text : null,
        status: "completed",
        verified: true,
      },
      { activity: true },
    ).catch((err) => storageNotice("history", err));
  }

  async function onOffer(session: PeerSession, offer: IncomingOffer) {
    const key = offer.peer.key;
    if (!settings.receiveEnabled) return offer.decline();
    const peer = peerRef(key, offer.peer);
    const trusted = !!known.get(key)?.trusted;

    // A message: shown right away, nothing to accept (LocalSend semantics).
    if (!offer.files.length) {
      const repeat = delivered.has(`${key} ${offer.transferId}`);
      if (!repeat && !admitMessage(key)) return offer.decline();
      try {
        offer.accept([]);
      } catch {
        return; // the sender cancelled meanwhile
      }
      deliverMessage(key, peer, trusted, offer.transferId, offer.text ?? "");
      return;
    }

    const record = resumeRecords.find((r) => r.key === key && r.transferId === offer.transferId && now() - r.at < RESUME_TTL_MS);
    const matches =
      record &&
      Object.entries(record.files).every(([id, m]) => offer.files.some((f) => f.id === id && f.size === m.size && f.name === m.name && f.mime === m.mime));
    if (record && matches) {
      // Its text (if any) was shown when it was first accepted.
      await resumeAccept(session, offer, record);
      return;
    }

    // One undecided offer per (peer, transfer), and bounded prompts per peer and overall.
    const waiting = [...pending.values()];
    if (
      waiting.some((p) => p.offer.peer.key === key && p.offer.transferId === offer.transferId) ||
      waiting.filter((p) => p.offer.peer.key === key).length >= MAX_PENDING_PER_PEER ||
      waiting.length >= MAX_PENDING
    ) {
      return offer.decline();
    }

    const request: IncomingRequest = {
      id: randomId(),
      peer,
      files: offer.files.map((f) => ({ id: f.id, name: f.name, size: f.size, mime: f.mime })),
      totalBytes: offer.files.reduce((n, f) => n + f.size, 0),
      // Text that comes with files is shown once the files are accepted (see `respond`).
      text: null,
      receivedAtMs: now(),
      trusted,
      defaultSaveDir: "This browser (Inbox)",
      expiresAtMs: offer.expiresAt ?? now() + settings.decisionTimeoutSecs * 1000,
    };
    pending.set(request.id, { request, offer, session });
    if (trusted && settings.autoAccept === "trusted") {
      void respond(request.id, { accept: null, decline: false, trust: false, saveDir: null });
      return;
    }
    emit({ type: "incomingRequest", request });
  }

  async function respond(requestId: string, decision: Decision): Promise<boolean> {
    const p = pending.get(requestId);
    if (!p) return false;
    pending.delete(requestId);
    // Accepting none of the files is a decline, as on the desktop: the text that
    // came with them is not shown either.
    const declined = decision.decline || (decision.accept != null && !p.offer.files.some((f) => decision.accept!.includes(f.id)));
    emit({ type: "incomingRequestClosed", id: requestId, reason: declined ? "declined" : "accepted" });
    if (declined) {
      try {
        p.offer.decline();
      } catch {
        /* the sender cancelled meanwhile */
      }
      return true;
    }
    const key = p.offer.peer.key;
    const wireId = p.offer.transferId;
    const ids = decision.accept ?? p.offer.files.map((f) => f.id);
    const files = p.offer.files.filter((f) => ids.includes(f.id));
    if (decision.trust) await remember(key, { trusted: true });
    const nonce = randomId();
    nonces.set(`${key} ${wireId}`, nonce);
    await rememberResume({
      key,
      transferId: wireId,
      files: Object.fromEntries(files.map((f) => [f.id, { name: f.name, size: f.size, mime: f.mime }])),
      nonce,
      at: now(),
    }).catch((err) => storageNotice("inbox", err));
    const t = track("receive", p.request.peer, files, null, null, wireId, p.session, key);
    t.files.forEach((f) => (f.state = "transferring"));
    t.samples = [{ at: now(), bytes: 0 }];
    update(t, { state: "transferring", saveDir: null });
    try {
      p.offer.accept(ids);
    } catch (err) {
      void forgetResume(key, wireId);
      finish(t, "failed", fail("accept_failed", err instanceof Error ? err.message : String(err)));
      return false;
    }
    // Accepted, even in part: the message that came with the files arrives too.
    if (p.offer.text !== undefined) deliverMessage(key, p.request.peer, p.request.trusted, wireId, p.offer.text);
    return true;
  }

  async function send(targets: SendTarget[], items: OutgoingItem[]): Promise<string[]> {
    const files: SourceFile[] = [];
    const texts: string[] = [];
    for (const item of items) {
      if (item.kind === "text") texts.push(item.text);
      else if (item.kind === "file") files.push(source(String(files.length), item.relativePath ?? item.name, item.file));
      else if (item.kind === "folder") for (const f of item.files) files.push(source(String(files.length), f.path, f.file));
      else throw unsupported("Sending files by path");
    }
    const bad = files.find((f) => fileNameProblem(f.name));
    if (bad) throw fail("bad_name", `"${bad.name}" can't be sent: ${fileNameProblem(bad.name)}.`);
    const text = texts.length ? texts.join("\n\n") : undefined;
    const dropId = targets.length > 1 ? randomId() : null;

    return targets.map((target) => {
      const key = target.id;
      const wireId = randomId();
      const t = track("send", peerRef(key), files, text ?? null, dropId, wireId, null, key);
      t.request = { transferId: wireId, files, text };
      void runSend(t, false);
      return t.summary.id;
    });
  }

  /** Offers `t` (again). A retry uses a fresh session: ids can't repeat within one. */
  async function runSend(t: Tracked, fresh: boolean) {
    const request = t.request!;
    try {
      const session = fresh ? await freshSession(t.key) : await sessionFor(t.key);
      if (isFinal(t.summary.state) || t.userCancelled) return;
      attach(t, session);
      update(t, { state: t.summary.bytesDone > 0 ? "transferring" : "waitingForAcceptance", error: null });
      const outcome = await session.sendTransfer(request);
      if (t.session !== session) return;
      if (outcome.declined) return finish(t, "declined", fail("declined", `${t.summary.peer.alias} declined.`));
      if (outcome.failed.length) {
        return finish(t, outcome.completed.length ? "completedWithErrors" : "failed", fail("files_failed", `${outcome.failed.length} file(s) failed.`));
      }
      finish(t, "completed", null);
    } catch (err) {
      if (isFinal(t.summary.state)) return;
      if (t.userCancelled) return finish(t, "cancelled", null);
      const e = err as { code?: string; message?: string; hint?: string };
      if (e.code && FATAL_CODES.has(e.code)) return finish(t, "failed", fail(e.code, e.message ?? String(err), e.hint));
      interrupted(t);
    }
  }

  async function freshSession(key: string): Promise<PeerSession> {
    const peer = present.get(key);
    if (!peer || !connector) throw fail("offline", `${summary(key)?.alias ?? "That device"} is offline.`);
    const session = await connector.connect(peer.id, undefined, { expectedPeerKey: key, roomSecret: roomOf(key)?.secret });
    await session.ready;
    const current = sessions.get(key);
    if (!current || current.state !== "ready") sessions.set(key, session);
    return session;
  }

  /** The connection dropped mid-transfer: reconnect and continue where it stopped. */
  function interrupted(t: Tracked) {
    if (isFinal(t.summary.state)) return;
    const alias = t.summary.peer.alias;
    if (t.summary.direction === "send" && t.request) {
      if (t.retrying) return;
      t.retrying = true;
      update(t, { state: "reconnecting", speedBps: 0, etaSecs: null, error: fail("connection_lost", "Connection lost. Reconnecting…", "It continues where it stopped.") });
      void retrySend(t);
    } else {
      if (t.giveUp) clearTimeout(t.giveUp);
      update(t, { state: "reconnecting", speedBps: 0, etaSecs: null, error: fail("connection_lost", `Waiting for ${alias} to reconnect…`, "It continues where it stopped.") });
      t.giveUp = setTimeout(
        () => finish(t, "failed", fail("connection_lost", "The connection was lost.", `When ${alias} sends again, it continues where it stopped.`)),
        RECEIVER_WAIT_MS,
      );
    }
  }

  async function retrySend(t: Tracked) {
    const delays = options.retryDelaysMs ?? RETRY_DELAYS_MS;
    for (const delay of delays) {
      await sleep(delay);
      if (isFinal(t.summary.state) || t.userCancelled) return;
      if (!present.has(t.key)) continue;
      t.attempts = (t.attempts ?? 0) + 1;
      if (t.attempts > delays.length) break;
      t.retrying = false;
      return runSend(t, true);
    }
    t.retrying = false;
    finish(t, "failed", fail("connection_lost", `Couldn't reach ${t.summary.peer.alias} again.`, "Press Try again when it's back online; it continues where it stopped."));
  }

  // ── Resume records (receiver) ───────────────────────────────────────────

  function liveRecords(list: unknown): ResumeRecord[] {
    return (Array.isArray(list) ? (list as ResumeRecord[]) : []).filter((r) => r && now() - r.at < RESUME_TTL_MS && r.nonce && r.files);
  }

  /** Read, change and write the shared list in one transaction, so tabs don't overwrite each other's records. */
  async function changeResume(fn: (list: ResumeRecord[]) => ResumeRecord[]) {
    const next = await kv.update("resume", (list) => fn(liveRecords(list)).slice(0, 50));
    resumeRecords = liveRecords(next);
  }

  async function rememberResume(record: ResumeRecord) {
    resumeRecords = [record, ...resumeRecords.filter((r) => !(r.key === record.key && r.transferId === record.transferId))];
    await changeResume((list) => [record, ...list.filter((r) => !(r.key === record.key && r.transferId === record.transferId))]);
  }

  async function forgetResume(key: string, transferId: string) {
    nonces.delete(`${key} ${transferId}`);
    resumeRecords = resumeRecords.filter((r) => !(r.key === key && r.transferId === transferId));
    await changeResume((list) => list.filter((r) => !(r.key === key && r.transferId === transferId))).catch(() => {});
  }

  /** A transfer we already accepted comes back after an interruption: continue without asking again. */
  async function resumeAccept(session: PeerSession, offer: IncomingOffer, record: ResumeRecord) {
    const ids = Object.keys(record.files);
    const wk = wireKey(record.key, "receive", offer.transferId);
    let t = byWire.get(wk);
    if (t && isFinal(t.summary.state)) t = undefined;
    // Take it over from an older session first, so nothing that one still reports touches it.
    const stale = t?.session && t.session !== session ? t.session : null;
    if (t) {
      if (t.giveUp) clearTimeout(t.giveUp);
      t.giveUp = undefined;
      attach(t, session);
    }
    if (stale && stale.state !== "closed") {
      // The sender moved to a new connection: end the old one (its partial file is kept) and let it settle.
      stale.close("resumed on a new connection");
      await sleep(100);
    }
    const context = { transferId: offer.transferId, peerKey: record.key };
    nonces.set(`${record.key} ${offer.transferId}`, record.nonce);
    const offsets: Record<string, number> = {};
    for (const id of ids) {
      const size = await storedSize(context, id, record.nonce).catch(() => 0);
      if (size > 0) offsets[id] = Math.min(size, record.files[id]!.size);
    }
    // Another offer for it may have taken over while the sizes were read.
    if (t && t.session !== session) return;
    if (!t) {
      const files = offer.files.filter((f) => ids.includes(f.id));
      t = track("receive", peerRef(record.key, offer.peer), files, null, null, offer.transferId, session, record.key);
    }
    t.done.clear();
    for (const [id, offset] of Object.entries(offsets)) t.done.set(id, offset);
    const resumed = Object.values(offsets).reduce((a, b) => a + b, 0);
    t.files.forEach((f) => (f.state = "transferring"));
    t.samples = [{ at: now(), bytes: resumed }];
    update(t, { state: "transferring", bytesDone: resumed, error: null, finishedAtMs: null });
    await rememberResume({ ...record, at: now() }).catch(() => {});
    try {
      offer.accept(ids, offsets);
    } catch {
      // The partial data can't be read back: start these files over.
      try {
        offer.accept(ids);
        t.done.clear();
        update(t, { bytesDone: 0 });
      } catch (err) {
        finish(t, "failed", fail("accept_failed", err instanceof Error ? err.message : String(err)));
      }
    }
  }

  // ── Stored files ────────────────────────────────────────────────────────

  function entryFor(path: string): Promise<StoredEntry | undefined> {
    return historyDb.getBy("path", path);
  }

  async function cachePreview(path: string, name: string, mime: string) {
    if (previews.has(path) || !RASTER.test(mime)) return;
    try {
      previews.set(path, URL.createObjectURL(await readStored(path, name, mime)));
    } catch {
      /* gone */
    }
  }

  function dropPreview(path: string) {
    const url = previews.get(path);
    if (url) URL.revokeObjectURL(url);
    previews.delete(path);
  }

  async function fileAt(path: string): Promise<File> {
    const h = await entryFor(path);
    return readStored(path, h?.name.split("/").pop() ?? "file", h?.mime ?? "");
  }

  /** Every stored entry `filter` accepts, oldest first, read a page at a time. */
  async function scan(filter: (e: StoredEntry) => boolean, fn: (e: StoredEntry) => void | Promise<void>) {
    let after: number | undefined;
    for (;;) {
      const page = await historyDb.page({ limit: PAGE, after });
      for (const e of page) if (filter(e)) await fn(e);
      if (page.length < PAGE) return;
      after = page.at(-1)!.id;
    }
  }

  /** Deletes partial files of transfers that can no longer resume, and files nothing lists. */
  async function cleanUp() {
    // Read fresh: another tab may have received files or accepted transfers since this one started.
    const keep = new Set<string>();
    await scan(isInboxFile, (e) => {
      const id = e.path ? idOfPath(e.path) : null;
      if (id) keep.add(id);
    });
    for (const r of liveRecords(await kv.get("resume"))) {
      for (const id of Object.keys(r.files)) keep.add(await storageId({ peerKey: r.key, transferId: r.transferId }, id, r.nonce));
    }
    const result = await pruneStored(keep, { graceMs: PRUNE_GRACE_MS });
    if (result.failed) storageNotice("cleanup", new Error(`${result.failed} file(s) couldn't be deleted`));
  }

  // ── Shared into the PWA (Web Share Target, handled by the service worker) ─

  async function takeShared(): Promise<OutgoingItem[]> {
    if (!new URLSearchParams(location.search).has("shared") || !("caches" in window)) return [];
    const cache = await caches.open("ferry-share");
    const out: OutgoingItem[] = [];
    for (const req of await cache.keys()) {
      const res = await cache.match(req);
      if (!res) continue;
      const name = decodeURIComponent(res.headers.get("x-ferry-name") ?? "shared");
      if (res.headers.get("x-ferry-kind") === "text") {
        const text = await res.text();
        if (text.trim()) out.push({ kind: "text", text, name: "Text" });
      } else {
        const blob = await res.blob();
        const file = new File([blob], name, { type: blob.type });
        out.push({ kind: "file", file, name, size: file.size });
      }
      await cache.delete(req);
    }
    window.history.replaceState(null, "", location.pathname);
    return out;
  }

  // ── Platform ────────────────────────────────────────────────────────────

  function local(): LocalDevice {
    return {
      alias: settings.alias,
      fingerprint: publicKey,
      deviceKind: "web",
      deviceModel: browserName(),
      port: 0,
      protocol: "https",
      addresses: [],
      shortId,
      appVersion: __FERRY_VERSION__,
    };
  }

  return {
    capabilities,

    async init(): Promise<Snapshot> {
      const stored = deserializeFromIdb(await kv.get("identity"));
      if (stored) identity = stored;
      else {
        identity = await generateIdentity();
        await kv.put(serializeForIdb(identity), "identity");
      }
      publicKey = await exportPublicKey(identity);
      shortId = await shortIdOf(publicKey);
      settings = { ...defaultSettings(), ...((await kv.get("settings")) as Partial<Settings> | undefined) };
      for (const d of await knownDb.all()) known.set(d.id, d);
      resumeRecords = liveRecords(await kv.get("resume"));
      // Partial files of transfers that can no longer resume are unreachable: delete them.
      void cleanUp().catch((err) => storageNotice("cleanup", err));
      void historyDb
        .page({ limit: 40, reverse: true, filter: (e) => isInboxFile(e) && RASTER.test(e.mime) })
        .then((recent) => recent.forEach((h) => void cachePreview(h.path!, h.name, h.mime)))
        .catch(() => {});
      // Ask the browser not to evict received files under storage pressure.
      void navigator.storage?.persist?.().catch(() => false);
      startSignaling();
      // Opened from a private link (now, or later in this tab): join its room.
      // The secret lives only in the URL fragment, which browsers never send.
      const fromFragment = () => {
        if (!location.hash.includes("room=")) return;
        try {
          joinLink(location.hash);
          emit({ type: "notice", level: "info", code: "room_joined", message: "You opened a private link. Devices that open the same link appear here, on any network." });
        } catch {
          emit({ type: "notice", level: "warning", code: "bad_link", message: "That link isn't a valid Ferry link." });
        }
        window.history.replaceState(null, "", location.pathname + location.search);
      };
      setTimeout(fromFragment, 0);
      window.addEventListener("hashchange", fromFragment);
      return {
        local: local(),
        devices: [...known.keys()].map(summary).filter((d): d is DeviceSummary => !!d),
        transfers: [],
        settings: { ...settings },
        server: serverStatus(),
      };
    },

    subscribe(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },

    send,
    respond,
    async cancel(id) {
      const t = transfers.get(id);
      if (!t || isFinal(t.summary.state)) return false;
      t.userCancelled = true;
      t.session?.cancel("cancelled", t.wireId);
      if (t.summary.direction === "receive") void forgetResume(t.key, t.wireId);
      finish(t, "cancelled", null);
      return true;
    },
    /** Browser transfers can't be paused (`canPause` is false). */
    async pause() {
      return false;
    },
    /**
     * "Try again" for a send that failed because its connection was lost:
     * offers it again and the receiver continues where it stopped. Ignored
     * while a retry is already under way (the state is no longer "failed").
     */
    async resume(id) {
      const t = transfers.get(id);
      if (!t?.request || t.userCancelled || t.summary.direction !== "send" || t.summary.state !== "failed" || t.summary.error?.code !== "connection_lost") return false;
      t.attempts = 0;
      t.retrying = false;
      update(t, { state: "reconnecting", finishedAtMs: null, error: fail("connection_lost", "Reconnecting…", "It continues where it stopped.") });
      void runSend(t, true);
      return true;
    },
    async submitPin() {
      return false;
    },
    async dismiss(id) {
      const t = transfers.get(id);
      if (!t || !isFinal(t.summary.state)) return false;
      remove(t);
      return true;
    },
    async transferFiles(id) {
      return transfers.get(id)?.files.map((f) => ({ ...f })) ?? [];
    },

    async refreshDevices() {
      if (signalingState === "closed") startSignaling();
    },
    async addDevice() {
      throw fail("unsupported", "Browsers can't connect to an address directly.", "Open Ferry on the other device; it appears here when it uses the same signaling server.");
    },
    async setDeviceFlags(id, flags) {
      await remember(id, {
        ...(flags.trusted !== undefined ? { trusted: flags.trusted } : {}),
        ...(flags.favorite !== undefined ? { favorite: flags.favorite } : {}),
        ...(flags.customAlias !== undefined ? { customAlias: flags.customAlias } : {}),
      });
      return summary(id);
    },
    async forgetDevice(id) {
      known.delete(id);
      await knownDb.delete(id);
      announce(id);
    },

    async createPairingOffer() {
      throw unsupported("Pairing");
    },
    async cancelPairingOffer() {
      return false;
    },
    async pairWithUri() {
      throw unsupported("Pairing");
    },
    async startCodePairing() {
      throw unsupported("Pairing");
    },
    async cancelCodePairing() {
      return false;
    },
    async respondPairing() {
      return false;
    },
    async unpairDevice() {
      throw unsupported("Pairing");
    },

    /** The History page: newest first, paged by id (stable while entries arrive from this or another tab). */
    async history(limit, beforeId, direction) {
      const page = await historyDb.page({
        limit,
        after: beforeId ?? undefined,
        reverse: true,
        filter: (e) => isActivity(e) && (!direction || e.direction === direction),
      });
      return page.map(plain);
    },
    /** The Inbox: received files (whatever the history settings) and the messages history kept. */
    async inbox(limit: number, beforeId?: number) {
      const page = await historyDb.page({ limit, after: beforeId ?? undefined, reverse: true, filter: inInbox });
      return page.map(plain);
    },
    /** Removes one entry; a file received in this browser is deleted with it (nothing else could reach it). */
    async deleteHistory(id) {
      const h = await historyDb.get(id);
      if (!h) return false;
      if (h.path && isInboxFile(h)) {
        try {
          await deleteStored(h.path);
        } catch (err) {
          throw fail("delete_failed", `Couldn't delete "${h.name}" from this browser.`, `Try again. (${err instanceof Error ? err.message : String(err)})`);
        }
        dropPreview(h.path);
      }
      await historyDb.delete(id);
      return true;
    },
    /**
     * Clears the activity history (sent items, messages, the record of what
     * arrived). Received files stay in the Inbox: their entries lose only
     * their place in History. One transaction: all or nothing.
     */
    async clearHistory() {
      await historyDb.rewrite((e) => (isInboxFile(e) ? { ...e, inbox: true, activity: false } : null));
    },
    /** Deletes every file received in this browser. History entries stay, without the file. */
    async clearReceivedFiles() {
      const files: StoredEntry[] = [];
      await scan(isInboxFile, (e) => void files.push(e));
      let deleted = 0;
      let failed = 0;
      for (const e of files) {
        try {
          if (e.path) {
            await deleteStored(e.path);
            dropPreview(e.path);
          }
          if (isActivity(e)) await historyDb.put({ ...e, inbox: false, path: null });
          else await historyDb.delete(e.id);
          deleted++;
        } catch {
          failed++;
        }
      }
      return { deleted, failed };
    },

    async updateSettings(next) {
      const alias = next.alias.trim().slice(0, 64);
      if (!alias) throw fail("invalid_settings", "The device name can't be empty.");
      const previous = settings;
      // A plain copy: the UI hands in reactive proxies, which IndexedDB can't clone.
      const saved = JSON.parse(JSON.stringify({ ...next, alias, signalingUrl: next.signalingUrl?.trim() || null })) as Settings;
      await kv.put(saved, "settings");
      settings = saved;
      if (previous.signalingUrl !== settings.signalingUrl || previous.stunServers.join() !== settings.stunServers.join()) startSignaling();
      else if (previous.alias !== settings.alias) signaling?.update({ alias: settings.alias });
      emit({ type: "localDeviceChanged", device: local() });
      return { ...settings };
    },

    async shareWithBrowsers() {
      throw unsupported("Browser links");
    },
    async receiveFromBrowsers() {
      throw unsupported("Browser links");
    },
    async stopBrowserLink() {
      return false;
    },
    async browserLinks() {
      return [];
    },

    async diagnostics(): Promise<DiagnosticCheck[]> {
      const checks: DiagnosticCheck[] = [];
      checks.push({
        id: "signaling",
        label: "Signaling server",
        status: signalingState === "open" ? "ok" : signalingState === "connecting" ? "warning" : "error",
        value: signalingState === "open" ? "Connected" : signalingState === "connecting" ? "Connecting…" : "Not reachable",
        detail: `${signalingUrl(settings)}. Only connection setup goes through it; files go device to device.`,
      });
      checks.push({
        id: "nearby",
        label: "Devices found",
        status: present.size ? "ok" : "warning",
        value: String(present.size),
        detail: `Ferry browsers and apps on this network appear automatically${rooms.size ? `; ${rooms.size} private link(s) open` : ""}.`,
      });
      checks.push({
        id: "webrtc",
        label: "WebRTC",
        status: "RTCPeerConnection" in window ? "ok" : "error",
        value: "RTCPeerConnection" in window ? "Available" : "Not supported by this browser",
      });
      checks.push({
        id: "secure",
        label: "Secure context",
        status: window.isSecureContext ? "ok" : "error",
        value: window.isSecureContext ? "Yes" : "No. Open Ferry over https.",
      });
      const opfs = await hasOpfs();
      const estimate = await navigator.storage?.estimate?.().catch(() => null);
      const persisted = await navigator.storage?.persisted?.().catch(() => false);
      checks.push({
        id: "storage",
        label: "Storage for received files",
        status: opfs ? "ok" : "warning",
        value: opfs ? "Browser file storage" : "Limited (files up to 256 MB)",
        detail: estimate?.quota
          ? `${Math.round((estimate.usage ?? 0) / 1e6)} MB used of about ${Math.round(estimate.quota / 1e9)} GB${persisted ? ", protected from eviction" : ""}.`
          : undefined,
      });
      checks.push({
        id: "turn",
        label: "Relay (TURN)",
        status: signaling?.server?.caps.includes("turn") ? "ok" : "unknown",
        value: signaling?.server?.caps.includes("turn") ? "Available" : "Not offered by this server",
        detail: "Only used when a direct connection is impossible. Relayed data stays end-to-end encrypted.",
      });
      return checks;
    },

    async pickFiles() {
      const files = await pickInput({ multiple: true });
      return files ? itemsFromFileList(files) : null;
    },
    async pickFolder() {
      const files = await pickInput({ directory: true });
      return files ? itemsFromFileList(files) : null;
    },
    async pickSaveFolder() {
      return null;
    },
    async readClipboard() {
      try {
        const text = await navigator.clipboard.readText();
        if (text.trim()) return { kind: "text", text, name: "Clipboard" };
      } catch {
        /* denied or empty */
      }
      return null;
    },
    copyText: (text) => navigator.clipboard.writeText(text),
    async open(path) {
      const file = await fileAt(path);
      // The type came from the sender: only well-understood media types may
      // render in a tab on Ferry's origin; everything else is saved instead.
      if (!INLINE_SAFE.test(file.type)) return this.reveal(path);
      const type = file.type === "text/plain" ? "text/plain;charset=utf-8" : file.type;
      const url = URL.createObjectURL(new Blob([file], { type }));
      window.open(url, "_blank", "noopener");
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    },
    /** In the browser, "show in folder" means saving a copy to Downloads. */
    async reveal(path) {
      const file = await fileAt(path);
      const url = URL.createObjectURL(file);
      const a = document.createElement("a");
      a.href = url;
      a.download = file.name;
      a.rel = "noopener";
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    },
    previewUrl: (path) => previews.get(path) ?? null,
    async notify(title, body) {
      if (!("Notification" in window)) return;
      if (Notification.permission === "default") await Notification.requestPermission();
      if (Notification.permission === "granted") new Notification(title, { body });
    },

    takeShared,
    async createRoom() {
      return joinSecret(randomBytes(16));
    },
    async joinRoom(link: string) {
      return joinLink(link);
    },
    async leaveRoom(id: string) {
      return leave(id);
    },
    async rooms() {
      return [...rooms.values()].map(roomInfo);
    },
    async signalingStatus(): Promise<SignalingStatus> {
      return currentSignaling();
    },
  };
}
