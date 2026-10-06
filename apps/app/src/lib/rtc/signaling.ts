// Signaling client for `ferry-signal` / LocalSend's `/v1/ws` (05-protocol.md §5.1).
//
// Wire format: `wss://host/v1/ws?d=<base64url-nopad(JSON client info)>`, then
// JSON frames `{"type":"SCREAMING_SNAKE", ...}` in both directions. SDPs travel
// as base64url-nopad(zlib(SDP)) for LocalSend compatibility; this client
// exposes plain SDP strings and does the encoding itself. ICE candidates travel
// as flat `RTCIceCandidateInit` objects. Inbound and outbound frames are
// processed strictly in order (an OFFER is always on the wire before the ICE
// candidates that follow it). Frames that are not valid JSON or do not match
// the expected shape are ignored. Outbound values are checked against the
// server's limits first, because the server counts violations and disconnects
// repeat offenders. `fetchTurn()` reads the server's optional TURN credentials.

import { b64urlEncode, concatBytes, fromUtf8, tryB64urlDecode, utf8, utf8Length, type Bytes } from "./bytes";
import { Emitter } from "./emitter";
import { RtcError } from "./protocol";

export const SIGNALING_VERSION = "2.2";
export const SIGNALING_CAPS: readonly string[] = ["rooms", "trickle", "ferry-dc"];
/** Decompressed SDPs larger than this are rejected (decompression-bomb guard). */
export const MAX_SDP_BYTES = 256 * 1024;
/** Server limits (crates/ferry-signal): encoded `sdp`, serialized ICE `candidate`, `sessionId`, client-info fields. */
export const MAX_ENCODED_SDP_BYTES = 48 * 1024;
export const MAX_CANDIDATE_BYTES = 4 * 1024;
export const MAX_SESSION_ID_LENGTH = 64;
const MAX_ALIAS_CHARS = 64;
const MAX_DEVICE_MODEL_CHARS = 64;
const MAX_TOKEN_CHARS = 512;

export type SignalingDeviceType = "MOBILE" | "DESKTOP" | "WEB" | "HEADLESS" | "SERVER";

export interface ClientExt {
  v: number;
  caps: string[];
  /** Identity public key, base64url (Ed25519: 32 bytes, P-256: 65 bytes). */
  key: string;
  nearby?: boolean;
}

export interface ClientInfoWithoutId {
  alias: string;
  version: string;
  deviceModel?: string;
  deviceType?: string;
  token: string;
  ext?: ClientExt;
}

export interface ClientInfo extends ClientInfoWithoutId {
  id: string;
}

/** What this device announces. */
export interface SignalingInfo {
  /** Clipped to 64 characters (server limit). */
  alias: string;
  deviceModel?: string;
  /** Sent upper-cased ("WEB", "DESKTOP", …). */
  deviceType?: string;
  token: string;
  /** Identity public key, base64url. */
  publicKey: string;
  nearby?: boolean;
}

export interface ServerInfo {
  v: number;
  caps: string[];
}

/** `GET /v1/turn` answer: short-lived coturn credentials (`use-auth-secret`). */
export interface TurnCredentials {
  iceServers: RTCIceServer[];
  /** Validity in seconds. */
  ttl: number;
}

export interface SignalingErrorEvent {
  code: number | string;
  message: string;
  /** The relayed message (OFFER/ANSWER/ICE/CANCEL) this error is about. */
  sessionId?: string;
  /** The room (ROOM_JOIN) this error is about, e.g. 409 "room is full". */
  room?: string;
}

export type SignalingState = "connecting" | "open" | "closed";

export interface SignalingEvents {
  hello: { client: ClientInfo; peers: ClientInfo[]; server?: ServerInfo };
  join: { peer: ClientInfo };
  update: { peer: ClientInfo };
  left: { peerId: string };
  offer: { peer: ClientInfo; sessionId: string; sdp: string };
  answer: { peer: ClientInfo; sessionId: string; sdp: string };
  ice: { peer: ClientInfo; sessionId: string; candidate: RTCIceCandidateInit | null };
  cancel: { peer: ClientInfo; sessionId: string };
  roomHello: { room: string; peers: ClientInfo[] };
  roomPeerJoined: { room: string; peer: ClientInfo };
  roomPeerLeft: { room: string; peerId: string };
  /** Server `ERROR` frames, plus local problems (`code: "bad-sdp"`, `"connect"`). */
  error: SignalingErrorEvent;
  state: SignalingState;
}

/** The subset of WebSocket used here (injectable for tests). */
export interface WebSocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}

export type WebSocketFactory = new (url: string) => WebSocketLike;

export interface SignalingClientOptions {
  /** Endpoint, e.g. `wss://signal.example/v1/ws`. */
  url: string;
  info: SignalingInfo;
  WebSocket?: WebSocketFactory;
  /** First reconnect delay (default 1 s); doubles per failed attempt, capped at `maxBackoffMs`. */
  initialBackoffMs?: number;
  /** Reconnect delay cap (default 30 s). */
  maxBackoffMs?: number;
  /** Keepalive interval (default 25 s): `PING` to Ferry servers, an empty frame to plain LocalSend servers. */
  pingIntervalMs?: number;
  /** Drop and reconnect when a server that advertised `server` caps is silent this long (default 65 s). */
  idleTimeoutMs?: number;
  /** Jitter source in [0, 1) (default Math.random; jitter needs no crypto quality). */
  random?: () => number;
  /** HTTP client for `fetchTurn` (default globalThis.fetch). */
  fetch?: (url: string) => Promise<{ ok: boolean; status: number; json(): Promise<unknown> }>;
}

const WS_OPEN = 1;
const ROOM_ID = /^(?:r:[A-Za-z0-9_-]{16,64}|c:[0-9]{6})$/;
const noop = () => {};

/** `r:` + 16 to 64 base64url characters (link/QR rooms) or `c:` + 6 digits (short codes), as the server accepts. */
export function isValidRoomId(room: string): boolean {
  return typeof room === "string" && ROOM_ID.test(room);
}

export class SignalingClient extends Emitter<SignalingEvents> {
  private readonly url: string;
  private info: SignalingInfo;
  private readonly WS: WebSocketFactory;
  private readonly initialBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly pingIntervalMs: number;
  private readonly idleTimeoutMs: number;
  private readonly random: () => number;
  private readonly fetchFn: NonNullable<SignalingClientOptions["fetch"]>;

  private ws: WebSocketLike | null = null;
  private _state: SignalingState = "closed";
  private _client: ClientInfo | null = null;
  private _server: ServerInfo | null = null;
  private started = false;
  /** HELLO received on the current socket (rooms are joined after it). */
  private greeted = false;
  private attempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private lastInbound = 0;
  private readonly rooms = new Set<string>();
  private inbound: Promise<void> = Promise.resolve();
  private outbound: Promise<unknown> = Promise.resolve();

  constructor(options: SignalingClientOptions) {
    super();
    this.url = options.url;
    this.info = { ...options.info };
    this.WS = options.WebSocket ?? (globalThis.WebSocket as unknown as WebSocketFactory);
    this.initialBackoffMs = options.initialBackoffMs ?? 1000;
    this.maxBackoffMs = options.maxBackoffMs ?? 30_000;
    this.pingIntervalMs = options.pingIntervalMs ?? 25_000;
    this.idleTimeoutMs = options.idleTimeoutMs ?? 65_000;
    this.random = options.random ?? Math.random;
    this.fetchFn = options.fetch ?? ((url) => globalThis.fetch(url));
  }

  get state(): SignalingState {
    return this._state;
  }

  /** This device as the server sees it (from the last HELLO). */
  get client(): ClientInfo | null {
    return this._client;
  }

  /** Server extension info from the last HELLO, null for a plain LocalSend server. */
  get server(): ServerInfo | null {
    return this._server;
  }

  /** Rooms joined (re-joined automatically after every reconnect). */
  get joinedRooms(): string[] {
    return [...this.rooms];
  }

  /** Starts connecting (idempotent). Reconnects automatically until `close()`. */
  connect(): void {
    if (this.started) return;
    this.started = true;
    this.attempt = 0;
    this.open();
  }

  close(): void {
    this.started = false;
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.dropSocket();
    this.setState("closed");
  }

  /** Builds the connection URL (`?d=` carries the client info). */
  connectUrl(): string {
    const url = new URL(this.url);
    url.searchParams.set("d", b64urlEncode(utf8(JSON.stringify(this.clientInfo()))));
    return url.toString();
  }

  sendOffer(target: string, sessionId: string, sdp: string): Promise<void> {
    return this.sendSdp("OFFER", target, sessionId, sdp);
  }

  sendAnswer(target: string, sessionId: string, sdp: string): Promise<void> {
    return this.sendSdp("ANSWER", target, sessionId, sdp);
  }

  /** Trickles one ICE candidate; `null` signals end-of-candidates. */
  sendIce(target: string, sessionId: string, candidate: RTCIceCandidateInit | null): Promise<void> {
    const bad = checkSessionId(sessionId);
    if (bad) return Promise.reject(bad);
    const init = candidate === null ? null : candidateInit(candidate);
    if (init && utf8Length(JSON.stringify(init)) > MAX_CANDIDATE_BYTES) {
      return Promise.reject(new RtcError("too-large", "ICE candidate too large"));
    }
    return this.send(() => ({ type: "ICE", target, sessionId, candidate: init }));
  }

  sendCancel(target: string, sessionId: string): Promise<void> {
    const bad = checkSessionId(sessionId);
    if (bad) return Promise.reject(bad);
    return this.send(() => ({ type: "CANCEL", target, sessionId }));
  }

  /** Joins a room now (when connected) and after every reconnect. Throws `RtcError("invalid")` for malformed ids. */
  joinRoom(room: string): void {
    if (!isValidRoomId(room)) throw new RtcError("invalid", "invalid room id");
    this.rooms.add(room);
    if (this.greeted) this.send(() => ({ type: "ROOM_JOIN", room })).catch(noop);
  }

  leaveRoom(room: string): void {
    if (!this.rooms.delete(room)) return;
    if (this.greeted) this.send(() => ({ type: "ROOM_LEAVE", room })).catch(noop);
  }

  /**
   * Fetches TURN credentials (`GET /v1/turn?peer=<own id>`) when the server
   * advertised the "turn" capability; null otherwise. They expire after `ttl`
   * seconds, so fetch them per connection (see `PeerConnectorOptions.iceServers`).
   */
  async fetchTurn(): Promise<TurnCredentials | null> {
    const client = this._client;
    if (!client || !this._server?.caps.includes("turn")) return null;
    const url = new URL(this.url);
    url.protocol = url.protocol === "ws:" ? "http:" : "https:";
    url.pathname = url.pathname.replace(/\/ws\/?$/, "/turn");
    url.search = "";
    url.hash = "";
    url.searchParams.set("peer", client.id);
    const res = await this.fetchFn(url.toString());
    if (!res.ok) throw new RtcError("turn", `TURN credentials unavailable (HTTP ${res.status})`);
    const turn = parseTurn(await res.json());
    if (!turn) throw new RtcError("turn", "malformed TURN credentials");
    return turn;
  }

  /** Updates the announced info; sent now when connected and used for every reconnect. */
  update(info: Partial<SignalingInfo>): void {
    this.info = { ...this.info, ...info };
    if (this._state === "open") this.send(() => ({ type: "UPDATE", info: this.clientInfo() })).catch(noop);
  }

  // ── Connection management ───────────────────────────────────────────────

  private clientInfo(): ClientInfoWithoutId {
    const i = this.info;
    const ext: ClientExt = { v: 1, caps: [...SIGNALING_CAPS], key: i.publicKey };
    if (i.nearby !== undefined) ext.nearby = i.nearby;
    // Member order as upstream: alias, version, deviceModel, deviceType, token, ext.
    const out = { alias: clip(i.alias, MAX_ALIAS_CHARS), version: SIGNALING_VERSION } as ClientInfoWithoutId;
    if (i.deviceModel !== undefined) out.deviceModel = clip(i.deviceModel, MAX_DEVICE_MODEL_CHARS);
    if (i.deviceType !== undefined) out.deviceType = i.deviceType.toUpperCase();
    out.token = clip(i.token, MAX_TOKEN_CHARS);
    out.ext = ext;
    return out;
  }

  private setState(state: SignalingState): void {
    if (this._state === state) return;
    this._state = state;
    this.emit("state", state);
  }

  private open(): void {
    this.reconnectTimer = null;
    if (!this.started) return;
    this.setState("connecting");
    this.greeted = false;
    let ws: WebSocketLike;
    try {
      ws = new this.WS(this.connectUrl());
    } catch (err) {
      this.emit("error", { code: "connect", message: err instanceof Error ? err.message : String(err) });
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      if (ws !== this.ws) return;
      this.lastInbound = Date.now();
      this.setState("open");
      this.pingTimer = setInterval(() => this.tick(), this.pingIntervalMs);
    };
    ws.onmessage = (ev) => {
      if (ws !== this.ws) return;
      this.lastInbound = Date.now();
      const data = ev.data;
      if (typeof data !== "string") return; // the protocol is text-only
      this.inbound = this.inbound.then(() => this.handle(data)).catch(noop);
    };
    ws.onerror = noop; // a close event always follows
    ws.onclose = () => {
      if (ws !== this.ws) return;
      this.dropSocket();
      this.setState("closed");
      this.scheduleReconnect();
    };
  }

  private tick(): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) return;
    if (this._server && Date.now() - this.lastInbound > this.idleTimeoutMs) {
      // The server pongs our pings; silence means a dead connection the browser has not noticed.
      this.dropSocket();
      this.setState("closed");
      this.scheduleReconnect();
      return;
    }
    try {
      // Plain LocalSend servers do not know PING; their own web client keeps alive with empty frames.
      ws.send(this._server ? JSON.stringify({ type: "PING" }) : "");
    } catch {
      // close follows
    }
  }

  private dropSocket(): void {
    this.greeted = false;
    if (this.pingTimer !== null) clearInterval(this.pingTimer);
    this.pingTimer = null;
    const ws = this.ws;
    this.ws = null;
    if (!ws) return;
    ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
    try {
      ws.close(1000);
    } catch {
      // ignore
    }
  }

  /** Exponential backoff with "equal jitter": delay ∈ [d/2, d), d = min(cap, initial · 2^attempt). */
  private scheduleReconnect(): void {
    if (!this.started || this.reconnectTimer !== null) return;
    const exp = Math.min(this.maxBackoffMs, this.initialBackoffMs * 2 ** this.attempt);
    const delay = Math.min(this.maxBackoffMs, Math.round(exp / 2 + (this.random() * exp) / 2));
    this.attempt++;
    this.reconnectTimer = setTimeout(() => this.open(), delay);
  }

  private sendSdp(type: "OFFER" | "ANSWER", target: string, sessionId: string, sdp: string): Promise<void> {
    const bad = checkSessionId(sessionId);
    if (bad) return Promise.reject(bad);
    return this.send(async () => {
      const encoded = await encodeSdp(sdp);
      if (encoded.length > MAX_ENCODED_SDP_BYTES) throw new RtcError("too-large", "SDP too large for the signaling server");
      return { type, target, sessionId, sdp: encoded };
    });
  }

  private send(build: () => object | Promise<object>): Promise<void> {
    const run = this.outbound.then(async () => {
      const msg = await build();
      const ws = this.ws;
      if (!ws || ws.readyState !== WS_OPEN || this._state !== "open") throw new RtcError("closed", "signaling is not connected");
      ws.send(JSON.stringify(msg));
    });
    this.outbound = run.catch(noop);
    return run;
  }

  // ── Inbound ─────────────────────────────────────────────────────────────

  private async handle(text: string): Promise<void> {
    let raw: unknown;
    try {
      raw = JSON.parse(text);
    } catch {
      return; // not JSON (upstream servers sometimes send bare strings)
    }
    if (!isObj(raw) || typeof raw.type !== "string") return;
    switch (raw.type) {
      case "HELLO": {
        const client = parseClient(raw.client);
        if (!client || !Array.isArray(raw.peers)) return;
        const server = parseServer(raw.server);
        this._client = client;
        this._server = server ?? null;
        this.greeted = true;
        this.attempt = 0;
        for (const room of this.rooms) this.send(() => ({ type: "ROOM_JOIN", room })).catch(noop);
        this.emit("hello", server ? { client, peers: parseClients(raw.peers), server } : { client, peers: parseClients(raw.peers) });
        return;
      }
      case "JOIN":
      case "UPDATE": {
        const peer = parseClient(raw.peer);
        if (peer) this.emit(raw.type === "JOIN" ? "join" : "update", { peer });
        return;
      }
      case "LEFT":
        if (isStr(raw.peerId, 1, 256)) this.emit("left", { peerId: raw.peerId });
        return;
      case "OFFER":
      case "ANSWER": {
        const peer = parseClient(raw.peer);
        if (!peer || !isStr(raw.sessionId, 1, 256) || !isStr(raw.sdp, 1, MAX_SDP_BYTES * 2)) return;
        let sdp: string;
        try {
          sdp = await decodeSdp(raw.sdp);
        } catch (err) {
          this.emit("error", { code: "bad-sdp", message: err instanceof Error ? err.message : String(err), sessionId: raw.sessionId });
          return;
        }
        this.emit(raw.type === "OFFER" ? "offer" : "answer", { peer, sessionId: raw.sessionId, sdp });
        return;
      }
      case "ICE": {
        const peer = parseClient(raw.peer);
        const candidate = parseCandidate(raw.candidate);
        if (!peer || !isStr(raw.sessionId, 1, 256) || candidate === undefined) return;
        this.emit("ice", { peer, sessionId: raw.sessionId, candidate });
        return;
      }
      case "CANCEL": {
        const peer = parseClient(raw.peer);
        if (peer && isStr(raw.sessionId, 1, 256)) this.emit("cancel", { peer, sessionId: raw.sessionId });
        return;
      }
      case "ROOM_HELLO":
        if (isStr(raw.room, 1, 256) && Array.isArray(raw.peers)) this.emit("roomHello", { room: raw.room, peers: parseClients(raw.peers) });
        return;
      case "ROOM_PEER_JOINED": {
        const peer = parseClient(raw.peer);
        if (peer && isStr(raw.room, 1, 256)) this.emit("roomPeerJoined", { room: raw.room, peer });
        return;
      }
      case "ROOM_PEER_LEFT":
        if (isStr(raw.room, 1, 256) && isStr(raw.peerId, 1, 256)) this.emit("roomPeerLeft", { room: raw.room, peerId: raw.peerId });
        return;
      case "ERROR": {
        const code = raw.code;
        if (typeof code !== "number" && typeof code !== "string") return;
        const ev: SignalingErrorEvent = { code, message: typeof raw.message === "string" ? raw.message.slice(0, 1024) : "" };
        if (isStr(raw.sessionId, 1, 256)) ev.sessionId = raw.sessionId;
        if (isStr(raw.room, 1, 256)) ev.room = raw.room;
        this.emit("error", ev);
        return;
      }
      default:
        return; // PONG and unknown types
    }
  }
}

// ── SDP and candidate encoding ────────────────────────────────────────────

/** base64url-nopad(zlib-deflate(UTF-8 SDP)), as LocalSend sends it. */
export async function encodeSdp(sdp: string): Promise<string> {
  return b64urlEncode(await pipe(utf8(sdp), new CompressionStream("deflate"), Infinity));
}

/** Inverse of `encodeSdp`. Tolerates padding and the standard alphabet; throws on corrupt input. */
export async function decodeSdp(encoded: string): Promise<string> {
  const normalized = encoded.replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  const bytes = tryB64urlDecode(normalized);
  if (!bytes) throw new TypeError("SDP is not base64url");
  return fromUtf8(await pipe(bytes, new DecompressionStream("deflate"), MAX_SDP_BYTES));
}

/** The flat `RTCIceCandidateInit` sent in `ICE` (only the four standard members). */
function candidateInit(candidate: RTCIceCandidateInit): RTCIceCandidateInit {
  const c: RTCIceCandidateInit = { candidate: candidate.candidate ?? "" };
  if (candidate.sdpMid !== undefined) c.sdpMid = candidate.sdpMid;
  if (candidate.sdpMLineIndex !== undefined) c.sdpMLineIndex = candidate.sdpMLineIndex;
  if (candidate.usernameFragment !== undefined) c.usernameFragment = candidate.usernameFragment;
  return c;
}

async function pipe(input: Bytes, stream: CompressionStream | DecompressionStream, limit: number): Promise<Bytes> {
  const writer = stream.writable.getWriter();
  const reader = stream.readable.getReader();
  // Write and read concurrently; write errors resurface through the reader.
  writer
    .write(input)
    .then(() => writer.close())
    .catch(noop);
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { value, done } = (await reader.read()) as ReadableStreamReadResult<Uint8Array>;
    if (done) break;
    total += value.byteLength;
    if (total > limit) {
      await reader.cancel().catch(noop);
      throw new RangeError("SDP too large");
    }
    chunks.push(value);
  }
  return concatBytes(...chunks);
}

// ── Validation helpers ────────────────────────────────────────────────────

type Obj = Record<string, unknown>;

function isObj(value: unknown): value is Obj {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStr(value: unknown, min: number, max: number): value is string {
  return typeof value === "string" && value.length >= min && value.length <= max;
}

/** At most `max` code points (the server counts characters, not UTF-16 units). */
function clip(value: string, max: number): string {
  if (value.length <= max) return value;
  return Array.from(value).slice(0, max).join("");
}

function checkSessionId(sessionId: string): RtcError | null {
  return isStr(sessionId, 1, MAX_SESSION_ID_LENGTH) ? null : new RtcError("invalid", "invalid session id");
}

function parseClient(value: unknown): ClientInfo | null {
  if (!isObj(value)) return null;
  const { id, alias, version, token } = value;
  if (!isStr(id, 1, 256) || !isStr(alias, 0, 256) || !isStr(version, 0, 32) || !isStr(token, 0, 1024)) return null;
  const info: ClientInfo = { id, alias, version, token };
  if (isStr(value.deviceModel, 0, 256)) info.deviceModel = value.deviceModel;
  if (isStr(value.deviceType, 0, 32)) info.deviceType = value.deviceType;
  const ext = value.ext;
  if (
    isObj(ext) &&
    typeof ext.v === "number" &&
    Array.isArray(ext.caps) &&
    ext.caps.length <= 64 &&
    ext.caps.every((c) => isStr(c, 0, 64)) &&
    isStr(ext.key, 1, 256)
  ) {
    info.ext = { v: ext.v, caps: ext.caps as string[], key: ext.key };
    if (typeof ext.nearby === "boolean") info.ext.nearby = ext.nearby;
  }
  return info;
}

function parseClients(values: unknown[]): ClientInfo[] {
  const out: ClientInfo[] = [];
  for (const v of values) {
    const c = parseClient(v);
    if (c) out.push(c);
  }
  return out;
}

function parseTurn(value: unknown): TurnCredentials | null {
  if (!isObj(value) || typeof value.ttl !== "number" || !Array.isArray(value.iceServers)) return null;
  const iceServers: RTCIceServer[] = [];
  for (const s of value.iceServers) {
    if (!isObj(s)) return null;
    const urls = typeof s.urls === "string" ? [s.urls] : s.urls;
    if (!Array.isArray(urls) || urls.length === 0 || !urls.every((u) => isStr(u, 1, 1024) && /^(?:turns?|stun):/.test(u))) return null;
    if (!isStr(s.username, 1, 1024) || !isStr(s.credential, 1, 1024)) return null;
    iceServers.push({ urls: urls as string[], username: s.username, credential: s.credential });
  }
  return { iceServers, ttl: value.ttl };
}

function parseServer(value: unknown): ServerInfo | undefined {
  if (!isObj(value) || typeof value.v !== "number" || !Array.isArray(value.caps)) return undefined;
  return { v: value.v, caps: value.caps.filter((c): c is string => typeof c === "string") };
}

/**
 * `null` = end of candidates; `undefined` = invalid. The wire value is a flat
 * `RTCIceCandidateInit` object; a bare `candidate:` line (which the server also
 * relays) is taken to belong to the only m-line of a data-channel-only SDP.
 */
function parseCandidate(value: unknown): RTCIceCandidateInit | null | undefined {
  if (value === null) return null;
  if (typeof value === "string") {
    return value.startsWith("candidate:") && value.length <= MAX_CANDIDATE_BYTES ? { candidate: value, sdpMLineIndex: 0 } : undefined;
  }
  const raw = value;
  if (!isObj(raw) || !isStr(raw.candidate, 0, MAX_CANDIDATE_BYTES)) return undefined;
  const c: RTCIceCandidateInit = { candidate: raw.candidate };
  if (raw.sdpMid === null || isStr(raw.sdpMid, 0, 256)) c.sdpMid = raw.sdpMid;
  if (raw.sdpMLineIndex === null || (typeof raw.sdpMLineIndex === "number" && Number.isInteger(raw.sdpMLineIndex))) {
    c.sdpMLineIndex = raw.sdpMLineIndex;
  }
  if (raw.usernameFragment === null || isStr(raw.usernameFragment, 0, 256)) c.usernameFragment = raw.usernameFragment;
  return c;
}
