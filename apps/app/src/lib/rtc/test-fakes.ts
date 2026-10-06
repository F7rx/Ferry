// Test doubles for the rtc library (imported by *.test.ts only).

import { b64urlDecode, concatBytes, fromUtf8, randomBytes, randomId, toHex, utf8Length } from "./bytes";
import type { Emitter } from "./emitter";
import { generateIdentity, type Identity } from "./identity";
import { BUFFER_HIGH_WATER, type FileMeta } from "./protocol";
import {
  PeerSession,
  type DataChannelLike,
  type FileSink,
  type PeerSessionOptions,
  type SinkAbortReason,
  type SinkWriter,
  type SourceFile,
} from "./session";
import type { ClientInfo, WebSocketFactory, WebSocketLike } from "./signaling";

// ── Data channel pair ─────────────────────────────────────────────────────

export interface PairOptions {
  /** Bytes the simulated network moves per tick (default 512 KiB). */
  bytesPerTick?: number;
  /** Delay between ticks (default 0 → next macrotask). */
  tickMs?: number;
}

type Frame = string | ArrayBuffer;

/**
 * In-memory RTCDataChannel: `send` copies the data and grows `bufferedAmount`;
 * a pump delivers frames asynchronously, in order, at a bounded rate, and
 * fires `bufferedamountlow` when the buffer drains past the threshold.
 */
export class FakeDataChannel implements DataChannelLike {
  readyState: RTCDataChannelState = "connecting";
  bufferedAmount = 0;
  bufferedAmountLowThreshold = 0;
  binaryType: BinaryType = "blob";
  label = "ferry/1";
  ordered = true;
  maxRetransmits: number | null = null;
  maxPacketLifeTime: number | null = null;
  onopen: ((ev: Event) => unknown) | null = null;
  onclose: ((ev: Event) => unknown) | null = null;
  onmessage: ((ev: MessageEvent) => unknown) | null = null;
  onbufferedamountlow: ((ev: Event) => unknown) | null = null;

  peer!: FakeDataChannel;
  /** Largest `bufferedAmount` ever observed (right after a send). */
  maxBuffered = 0;
  /** Largest `bufferedAmount` observed right *before* a binary send (backpressure gauge). */
  maxBufferedBeforeBinarySend = 0;
  maxBinaryFrame = 0;
  binaryFrames = 0;
  binaryBytes = 0;
  /** Every text frame sent, in order. */
  readonly sentText: string[] = [];
  /** Drop frames instead of delivering them (simulated network loss). */
  blackhole = false;
  /** Hold frames in the buffer (no delivery) until `resume()`. */
  paused = false;
  /** Rewrites (or drops, with null) each frame this end sends, right before delivery. */
  transform: ((frame: Frame) => Frame | null) | null = null;

  private readonly queue: Frame[] = [];
  private scheduled = false;
  private closing = false;

  constructor(
    private readonly bytesPerTick: number,
    private readonly tickMs: number,
  ) {}

  send(data: string): void;
  send(data: ArrayBufferView<ArrayBuffer>): void;
  send(data: string | ArrayBufferView<ArrayBuffer>): void {
    if (this.readyState !== "open") throw new Error("InvalidStateError: channel is not open");
    let frame: Frame;
    let size: number;
    if (typeof data === "string") {
      frame = data;
      size = utf8Length(data);
      this.sentText.push(data);
    } else {
      this.maxBufferedBeforeBinarySend = Math.max(this.maxBufferedBeforeBinarySend, this.bufferedAmount);
      frame = data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength);
      size = frame.byteLength;
      this.binaryFrames++;
      this.binaryBytes += size;
      this.maxBinaryFrame = Math.max(this.maxBinaryFrame, size);
    }
    this.queue.push(frame);
    this.bufferedAmount += size;
    this.maxBuffered = Math.max(this.maxBuffered, this.bufferedAmount);
    this.schedule();
  }

  close(): void {
    if (this.readyState === "closing" || this.readyState === "closed") return;
    this.readyState = "closing";
    this.closing = true;
    if (this.queue.length === 0) this.finishClose();
    else this.schedule();
  }

  resume(): void {
    this.paused = false;
    this.schedule();
  }

  /** Text frames sent with the given control type (`{"t":type,…}`), parsed. */
  sentControl(type: string): Record<string, unknown>[] {
    return this.sentText.map((t) => JSON.parse(t) as Record<string, unknown>).filter((m) => m.t === type);
  }

  private schedule(): void {
    if (this.scheduled) return;
    this.scheduled = true;
    setTimeout(() => this.pump(), this.tickMs);
  }

  private pump(): void {
    this.scheduled = false;
    if (this.paused) return;
    let budget = this.bytesPerTick;
    while (this.queue.length > 0 && budget > 0) {
      const frame = this.queue.shift()!;
      const size = typeof frame === "string" ? utf8Length(frame) : frame.byteLength;
      budget -= size;
      const before = this.bufferedAmount;
      this.bufferedAmount -= size;
      const delivered = this.transform ? this.transform(frame) : frame;
      if (delivered !== null && !this.blackhole && this.peer.readyState === "open") {
        this.peer.onmessage?.({ data: delivered } as MessageEvent);
      }
      if (before > this.bufferedAmountLowThreshold && this.bufferedAmount <= this.bufferedAmountLowThreshold) {
        this.onbufferedamountlow?.(new Event("bufferedamountlow"));
      }
    }
    if (this.queue.length > 0) this.schedule();
    else if (this.closing) this.finishClose();
  }

  private finishClose(): void {
    setTimeout(() => {
      // Over a dead link (blackhole) the other end never learns about the close;
      // a channel whose connection never came up has no other end at all.
      for (const ch of this.blackhole || !this.peer ? [this] : [this, this.peer]) {
        if (ch.readyState === "closed") continue;
        ch.readyState = "closed";
        ch.queue.length = 0;
        ch.bufferedAmount = 0;
        ch.onclose?.(new Event("close"));
      }
    }, 0);
  }

  /** Opens both ends (fires `open` asynchronously on each). */
  static openPair(a: FakeDataChannel, b: FakeDataChannel): void {
    a.readyState = b.readyState = "open";
    setTimeout(() => {
      a.onopen?.(new Event("open"));
      b.onopen?.(new Event("open"));
    }, 0);
  }
}

export function createChannelPair(options: PairOptions = {}): [FakeDataChannel, FakeDataChannel] {
  const rate = options.bytesPerTick ?? 512 * 1024;
  const tick = options.tickMs ?? 0;
  const a = new FakeDataChannel(rate, tick);
  const b = new FakeDataChannel(rate, tick);
  a.peer = b;
  b.peer = a;
  return [a, b];
}

// ── Sessions ──────────────────────────────────────────────────────────────

/** A random, already normalized DTLS fingerprint (`extractFingerprint` format). */
export function fakeFingerprint(): string {
  return `sha-256 ${toHex(randomBytes(32)).toUpperCase().match(/../g)!.join(":")}`;
}

export interface SessionPairOptions {
  channel?: PairOptions;
  /** Overrides for the offerer (`a`) and the answerer (`b`). */
  a?: Partial<PeerSessionOptions>;
  b?: Partial<PeerSessionOptions>;
  identities?: [Identity, Identity];
  /** Leave the channels closed (call `FakeDataChannel.openPair` yourself). */
  manualOpen?: boolean;
}

export interface SessionPair {
  a: PeerSession;
  b: PeerSession;
  chA: FakeDataChannel;
  chB: FakeDataChannel;
  idA: Identity;
  idB: Identity;
  fpA: string;
  fpB: string;
}

/** Two sessions on a channel pair: `a` is the offerer, `b` the answerer. Not awaited: use `ready`. */
export async function createSessionPair(options: SessionPairOptions = {}): Promise<SessionPair> {
  const [chA, chB] = createChannelPair(options.channel);
  const [idA, idB] = options.identities ?? [await generateIdentity("ed25519"), await generateIdentity("ed25519")];
  const fpA = fakeFingerprint();
  const fpB = fakeFingerprint();
  const a = new PeerSession({
    channel: chA,
    role: "offerer",
    sessionId: "session-1",
    localFingerprint: fpA,
    remoteFingerprint: fpB,
    identity: idA,
    device: { alias: "Alice", deviceType: "web", platform: "test" },
    maxMessageSize: 262144,
    ...options.a,
  });
  const b = new PeerSession({
    channel: chB,
    role: "answerer",
    sessionId: "session-1",
    localFingerprint: fpB,
    remoteFingerprint: fpA,
    identity: idB,
    device: { alias: "Bob", deviceType: "desktop", platform: "test" },
    maxMessageSize: 262144,
    ...options.b,
  });
  if (!options.manualOpen) FakeDataChannel.openPair(chA, chB);
  return { a, b, chA, chB, idA, idB, fpA, fpB };
}

// ── Files ─────────────────────────────────────────────────────────────────

export interface MemorySource extends SourceFile {
  /** Every [start, end) range read through `slice`. */
  reads: [number, number][];
  /** Make reads fail from now on. */
  failReads: boolean;
}

export function memorySource(id: string, data: Uint8Array, name = `${id}.bin`, mime = "application/octet-stream"): MemorySource {
  const source: MemorySource = {
    id,
    name,
    size: data.byteLength,
    mime,
    reads: [],
    failReads: false,
    slice: async (start, end) => {
      source.reads.push([start, end]);
      if (source.failReads) throw new Error("NotReadableError: file changed");
      return data.slice(start, end).buffer;
    },
  };
  return source;
}

export interface MemoryFile {
  meta: FileMeta;
  offset: number;
  chunks: Uint8Array[];
  state: "open" | "closed" | "aborted";
  abortReason?: SinkAbortReason;
}

/** FileSink keeping data in memory (fine for tests; real sinks stream to disk). */
export class MemorySink implements FileSink {
  readonly files = new Map<string, MemoryFile>();
  /** Data already stored from an earlier, interrupted transfer (by file id). */
  readonly stored = new Map<string, Uint8Array>();
  readonly prefixReads: [string, number][] = [];
  /** File ids whose writes fail. */
  readonly failWrites = new Set<string>();
  /** Called at the start of every write (e.g. to sample session internals). */
  onWrite: ((meta: FileMeta, chunk: Uint8Array) => void) | null = null;
  /** Delay of every write (simulates a slow disk). */
  writeDelayMs = 0;
  private gate: Promise<void> | null = null;

  /** Blocks every write until the returned function is called. */
  blockWrites(): () => void {
    let release!: () => void;
    this.gate = new Promise((resolve) => {
      release = resolve;
    });
    return () => {
      this.gate = null;
      release();
    };
  }

  async open(meta: FileMeta, offset: number): Promise<SinkWriter> {
    const prefix = (this.stored.get(meta.id) ?? new Uint8Array(0)).slice(0, offset);
    const file: MemoryFile = { meta, offset, chunks: [prefix], state: "open" };
    this.files.set(meta.id, file);
    return {
      write: async (chunk) => {
        this.onWrite?.(meta, chunk);
        if (this.gate) await this.gate;
        if (this.writeDelayMs > 0) await sleep(this.writeDelayMs);
        if (file.state !== "open") throw new Error("writer is not open");
        if (this.failWrites.has(meta.id)) throw new Error("disk full");
        file.chunks.push(chunk.slice());
      },
      close: async () => {
        file.state = "closed";
      },
      abort: async (reason) => {
        file.state = "aborted";
        file.abortReason = reason;
      },
    };
  }

  async *readPrefix(meta: FileMeta, offset: number): AsyncIterable<Uint8Array> {
    this.prefixReads.push([meta.id, offset]);
    const stored = this.stored.get(meta.id) ?? new Uint8Array(0);
    for (let o = 0; o < offset && o < stored.byteLength; o += 100_000) {
      yield stored.subarray(o, Math.min(offset, o + 100_000));
    }
  }

  data(id: string): Uint8Array {
    const file = this.files.get(id);
    return file ? concatBytes(...file.chunks) : new Uint8Array(0);
  }
}

// ── WebSocket ─────────────────────────────────────────────────────────────

export class FakeWebSocket implements WebSocketLike {
  static instances: FakeWebSocket[] = [];
  readyState = 0;
  readonly sent: string[] = [];
  closedByClient = false;
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  /** Server-side hooks (used by FakeSignalServer). */
  onClientSend: ((data: string) => void) | null = null;
  onClientClose: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  static get last(): FakeWebSocket {
    const ws = FakeWebSocket.instances.at(-1);
    if (!ws) throw new Error("no WebSocket was created");
    return ws;
  }

  send(data: string): void {
    if (this.readyState !== 1) throw new Error("InvalidStateError");
    this.sent.push(data);
    this.onClientSend?.(data);
  }

  close(): void {
    if (this.readyState >= 2) return;
    this.readyState = 3;
    this.closedByClient = true;
    this.onClientClose?.();
  }

  // Server side
  serverOpen(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  serverSend(message: unknown): void {
    if (this.readyState !== 1) return;
    this.onmessage?.({ data: typeof message === "string" ? message : JSON.stringify(message) });
  }

  serverSendRaw(data: unknown): void {
    this.onmessage?.({ data });
  }

  serverClose(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onerror?.({});
    this.onclose?.({ code: 1006, reason: "" });
  }

  sentJson(): Record<string, unknown>[] {
    return this.sent.filter((s) => s !== "").map((s) => JSON.parse(s) as Record<string, unknown>);
  }
}

// ── Signaling server ──────────────────────────────────────────────────────

interface ServerConn {
  info: ClientInfo;
  ws: FakeWebSocket;
  ext: boolean;
}

type Json = Record<string, unknown>;

/**
 * In-memory `ferry-signal`: one IP group (everyone sees everyone), rooms, and
 * relaying with the real server's wire rules (ICE `candidate` a string or a
 * flat object ≤ 4 KiB, or null; sessionId ≤ 64 chars; encoded SDP ≤ 48 KiB;
 * `ERROR` correlation).
 */
export class FakeSignalServer {
  readonly conns = new Map<string, ServerConn>();
  readonly rooms = new Map<string, Set<string>>();
  /** Every client frame (keepalive empty frames excluded), parsed. */
  readonly received: { from: string; msg: Json }[] = [];
  /** Rewrites a relayed OFFER/ANSWER/ICE/CANCEL before delivery (relays stay in order); null drops it. */
  relayHook: ((msg: Json, from: ClientInfo, to: ClientInfo) => Json | null | Promise<Json | null>) | null = null;
  /** Advertise extensions in HELLO (false = behave like a plain LocalSend server). */
  ext = true;
  /** A WebSocket class whose instances connect to this server. */
  readonly WebSocket: WebSocketFactory;
  private relayChain: Promise<void> = Promise.resolve();

  constructor() {
    const server = this;
    this.WebSocket = class extends FakeWebSocket {
      constructor(url: string) {
        super(url);
        server.accept(this);
      }
    };
  }

  /** Server-side disconnect of a client (the client will reconnect). */
  kick(clientId: string): void {
    const conn = this.conns.get(clientId);
    if (!conn) return;
    this.remove(conn);
    conn.ws.serverClose();
  }

  private accept(ws: FakeWebSocket): void {
    setTimeout(() => {
      if (ws.readyState !== 0) return;
      const d = new URL(ws.url).searchParams.get("d") ?? "";
      const raw = JSON.parse(fromUtf8(b64urlDecode(d))) as Json;
      const ext = this.ext && typeof raw.ext === "object" && raw.ext !== null;
      const info: ClientInfo = { id: randomId(), alias: String(raw.alias), version: String(raw.version), token: String(raw.token) };
      if (typeof raw.deviceModel === "string") info.deviceModel = raw.deviceModel;
      if (typeof raw.deviceType === "string") info.deviceType = raw.deviceType.toUpperCase();
      if (ext) info.ext = raw.ext as ClientInfo["ext"];
      const conn: ServerConn = { info, ws, ext };
      const peers = [...this.conns.values()].map((c) => c.info);
      this.conns.set(info.id, conn);
      ws.onClientSend = (data) => this.onMessage(conn, data);
      ws.onClientClose = () => this.remove(conn);
      ws.serverOpen();
      const hello: Json = { type: "HELLO", client: info, peers };
      if (ext) hello.server = { v: 1, caps: ["rooms", "trickle", "ferry-dc"] };
      ws.serverSend(hello);
      for (const other of peers) this.deliver(other.id, { type: "JOIN", peer: info });
    }, 0);
  }

  private remove(conn: ServerConn): void {
    if (this.conns.get(conn.info.id) !== conn) return;
    this.conns.delete(conn.info.id);
    for (const [room, members] of this.rooms) {
      if (!members.delete(conn.info.id)) continue;
      for (const id of members) this.deliver(id, { type: "ROOM_PEER_LEFT", room, peerId: conn.info.id });
    }
    for (const id of this.conns.keys()) this.deliver(id, { type: "LEFT", peerId: conn.info.id });
  }

  /** Asynchronous, in-order delivery (timers with equal delays fire in order). */
  private deliver(id: string, msg: Json): void {
    setTimeout(() => this.conns.get(id)?.ws.serverSend(msg), 0);
  }

  private error(conn: ServerConn, code: number, message: string, extra: Json = {}): void {
    this.deliver(conn.info.id, { type: "ERROR", code, message, ...extra });
  }

  private onMessage(conn: ServerConn, data: string): void {
    if (data.trim() === "") return; // LocalSend-style keepalive
    const msg = JSON.parse(data) as Json;
    this.received.push({ from: conn.info.id, msg });
    const extOnly = !["UPDATE", "OFFER", "ANSWER"].includes(String(msg.type));
    if (extOnly && !conn.ext) return this.error(conn, 403, "extension messages require ext in the client info");
    switch (msg.type) {
      case "PING":
        return this.deliver(conn.info.id, { type: "PONG" });
      case "UPDATE":
        return;
      case "ROOM_JOIN": {
        const room = String(msg.room);
        const members = this.rooms.get(room) ?? new Set<string>();
        this.rooms.set(room, members);
        const peers = [...members].map((id) => this.conns.get(id)!.info);
        members.add(conn.info.id);
        this.deliver(conn.info.id, { type: "ROOM_HELLO", room, peers });
        for (const p of peers) this.deliver(p.id, { type: "ROOM_PEER_JOINED", room, peer: conn.info });
        return;
      }
      case "ROOM_LEAVE": {
        const room = String(msg.room);
        const members = this.rooms.get(room);
        if (!members?.delete(conn.info.id)) return;
        for (const id of members) this.deliver(id, { type: "ROOM_PEER_LEFT", room, peerId: conn.info.id });
        return;
      }
      case "OFFER":
      case "ANSWER":
      case "ICE":
      case "CANCEL":
        return this.relay(conn, msg);
      default:
        return this.error(conn, 400, "unknown message type");
    }
  }

  private relay(conn: ServerConn, msg: Json): void {
    const sessionId = msg.sessionId;
    if (typeof sessionId !== "string" || sessionId.length === 0 || sessionId.length > 64) {
      return this.error(conn, 400, "invalid sessionId");
    }
    if ((msg.type === "OFFER" || msg.type === "ANSWER") && (typeof msg.sdp !== "string" || msg.sdp.length > 48 * 1024)) {
      return this.error(conn, 413, "sdp too large", { sessionId });
    }
    if (msg.type === "ICE" && msg.candidate !== undefined && msg.candidate !== null) {
      // As the real server: a string or a flat object, at most 4 KiB serialized, relayed verbatim.
      const c = msg.candidate;
      const flat =
        typeof c === "string" ||
        (typeof c === "object" && !Array.isArray(c) && Object.values(c as Json).every((v) => typeof v !== "object" || v === null));
      if (!flat) return this.error(conn, 400, "invalid candidate", { sessionId });
      if (JSON.stringify(c).length > 4096) return this.error(conn, 413, "candidate too large", { sessionId });
    }
    const target = this.conns.get(String(msg.target));
    if (!target) return this.error(conn, 404, "unknown target", { sessionId });
    const out: Json = { type: msg.type, peer: conn.info, sessionId };
    if (msg.type === "OFFER" || msg.type === "ANSWER") out.sdp = msg.sdp;
    if (msg.type === "ICE") out.candidate = msg.candidate ?? null;
    const hook = this.relayHook;
    this.relayChain = this.relayChain.then(async () => {
      const result = hook ? await hook(out, conn.info, target.info) : out;
      if (result) this.deliver(target.info.id, result);
    });
  }
}

// ── Peer connection ───────────────────────────────────────────────────────

interface FakeIceEvent {
  candidate: { toJSON(): RTCIceCandidateInit } | null;
}

/**
 * Just enough RTCPeerConnection for PeerConnector: SDPs carry a fingerprint
 * line and an instance id; once both sides have both descriptions the pair
 * "connects": DTLS fails (connectionState "failed") when a remote SDP's
 * fingerprint does not match the other side's certificate, and the offerer's
 * data channel is paired with a new channel announced through `ondatachannel`.
 */
export class FakePeerConnection {
  static readonly all = new Map<string, FakePeerConnection>();
  /** Network model for the data channels created on connect. */
  static channelOptions: PairOptions = {};

  readonly id = randomId();
  readonly fingerprint = fakeFingerprint();
  localDescription: RTCSessionDescriptionInit | null = null;
  remoteDescription: RTCSessionDescriptionInit | null = null;
  connectionState: RTCPeerConnectionState = "new";
  onicecandidate: ((ev: FakeIceEvent) => void) | null = null;
  ondatachannel: ((ev: { channel: FakeDataChannel }) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  /** Remote candidates added through `addIceCandidate` (null = end of candidates). */
  readonly remoteCandidates: (RTCIceCandidateInit | null)[] = [];
  channel: FakeDataChannel | null = null;
  private linked = false;

  constructor(readonly config?: RTCConfiguration) {
    FakePeerConnection.all.set(this.id, this);
  }

  createDataChannel(label: string, init: RTCDataChannelInit = {}): FakeDataChannel {
    const ch = FakePeerConnection.newChannel();
    ch.label = label;
    ch.ordered = init.ordered ?? true;
    ch.maxRetransmits = init.maxRetransmits ?? null;
    ch.maxPacketLifeTime = init.maxPacketLifeTime ?? null;
    this.channel = ch;
    return ch;
  }

  async createOffer(): Promise<RTCSessionDescriptionInit> {
    return { type: "offer", sdp: this.sdp() };
  }

  async createAnswer(): Promise<RTCSessionDescriptionInit> {
    return { type: "answer", sdp: this.sdp() };
  }

  async setLocalDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    this.localDescription = desc;
    setTimeout(() => {
      const candidate: RTCIceCandidateInit = { candidate: `candidate:1 1 udp 2122260223 192.0.2.${Math.floor(Math.random() * 250) + 1} 50000 typ host`, sdpMid: "0", sdpMLineIndex: 0 };
      this.onicecandidate?.({ candidate: { toJSON: () => candidate } });
      this.onicecandidate?.({ candidate: null });
    }, 0);
    this.link();
  }

  async setRemoteDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    if (!desc.sdp || !/^a=fingerprint:/m.test(desc.sdp)) throw new Error("InvalidAccessError: no fingerprint");
    this.remoteDescription = desc;
    this.link();
  }

  async addIceCandidate(candidate?: RTCIceCandidateInit): Promise<void> {
    this.remoteCandidates.push(candidate ?? null);
  }

  close(): void {
    this.connectionState = "closed";
    this.channel?.close();
  }

  private sdp(): string {
    return [
      "v=0",
      `o=- ${this.id} 2 IN IP4 127.0.0.1`,
      "s=-",
      "t=0 0",
      "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
      `a=x-fake-pc:${this.id}`,
      `a=fingerprint:${this.fingerprint}`,
      "a=max-message-size:262144",
      "",
    ].join("\r\n");
  }

  private link(): void {
    if (this.linked || !this.localDescription || !this.remoteDescription) return;
    const remoteId = /^a=x-fake-pc:(\S+)/m.exec(this.remoteDescription.sdp ?? "")?.[1];
    const remote = remoteId ? FakePeerConnection.all.get(remoteId) : undefined;
    if (!remote || remote.linked || !remote.localDescription || !remote.remoteDescription) return;
    this.linked = remote.linked = true;
    const seen = (sdp: string | undefined) => /^a=fingerprint:(.+?)\s*$/m.exec(sdp ?? "")?.[1];
    const dtlsOk = seen(this.remoteDescription.sdp) === remote.fingerprint && seen(remote.remoteDescription.sdp) === this.fingerprint;
    setTimeout(() => {
      for (const pc of [this, remote]) {
        if (pc.connectionState === "closed") continue;
        pc.connectionState = dtlsOk ? "connected" : "failed";
        pc.onconnectionstatechange?.();
      }
      if (!dtlsOk) return;
      const offerer = this.localDescription?.type === "offer" ? this : remote;
      const answerer = offerer === this ? remote : this;
      const a = offerer.channel;
      if (!a || answerer.connectionState === "closed") return;
      const b = FakePeerConnection.newChannel();
      b.label = a.label;
      b.ordered = a.ordered;
      b.maxRetransmits = a.maxRetransmits;
      b.maxPacketLifeTime = a.maxPacketLifeTime;
      a.peer = b;
      b.peer = a;
      answerer.channel = b;
      a.readyState = b.readyState = "open";
      answerer.ondatachannel?.({ channel: b });
      setTimeout(() => a.onopen?.(new Event("open")), 0);
    }, 0);
  }

  private static newChannel(): FakeDataChannel {
    const o = FakePeerConnection.channelOptions;
    return new FakeDataChannel(o.bytesPerTick ?? 512 * 1024, o.tickMs ?? 0);
  }
}

// ── Async helpers ─────────────────────────────────────────────────────────

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Resolves with the next `type` event (matching `filter`); rejects after `timeoutMs`. */
export function nextEvent<E extends object, K extends keyof E>(
  emitter: Emitter<E>,
  type: K,
  filter?: (payload: E[K]) => boolean,
  timeoutMs = 5000,
): Promise<E[K]> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      off();
      reject(new Error(`timed out waiting for "${String(type)}"`));
    }, timeoutMs);
    const off = emitter.on(type, (payload) => {
      if (filter && !filter(payload)) return;
      clearTimeout(timer);
      off();
      resolve(payload);
    });
  });
}

/** Records every `type` event. */
export function collect<E extends object, K extends keyof E>(emitter: Emitter<E>, type: K): E[K][] {
  const list: E[K][] = [];
  emitter.on(type, (payload) => list.push(payload));
  return list;
}

export async function waitUntil(condition: () => boolean, timeoutMs = 5000, what = "condition"): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`);
    await sleep(2);
  }
}

/** Highest `bufferedAmount` a sender may legitimately reach: the mark plus one chunk and some control frames. */
export const BUFFER_CEILING = BUFFER_HIGH_WATER + 64 * 1024 + 4096;
