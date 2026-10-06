// PeerSession: one authenticated ferry-dc/1 data channel (05-protocol.md §5.2).
//
// Works on any RTCDataChannel-like object, so it is unit-testable without
// WebRTC. Both sides send `hello` as soon as the channel opens, compute the
// transcript T over both DTLS fingerprints as *they* observed them, exchange
// `auth` signatures (plus a room MAC in secret rooms) and only then accept
// transfers. Each direction can run one transfer at a time; both directions
// may run concurrently (binary frames always belong to the sender's open file).
//
// Memory stays bounded on both ends. Senders read 1 MiB blocks (one block of
// read-ahead), pause while `bufferedAmount` is above the high-water mark and
// keep at most RECV_WINDOW file bytes beyond the receiver's last `progress`
// report in flight. Receivers fail the session when a peer ignores that window
// or floods control frames. Offers and answers that do not fit one 64 KiB
// control frame are split into several (`more: true`).
//
// Resume hashing: the full-file SHA-256 is always verified. A resumed sender
// re-reads [0, offset) through `SourceFile.slice`; a resumed receiver re-reads
// it through the optional `FileSink.readPrefix` hook (required for offsets > 0).

import { sha256 } from "@noble/hashes/sha2.js";
import { b64urlDecode, b64urlEncode, equalBytes, isArrayBuffer, randomBytes, toHex, utf8Length, type Bytes } from "./bytes";
import { Emitter } from "./emitter";
import { exportPublicKey, shortCode, sign, verify, type Identity, type IdentityAlg } from "./identity";
import {
  BUFFER_HIGH_WATER,
  BUFFER_LOW_WATER,
  chunkSizeFor,
  DC_CAPS,
  DC_VERSION,
  encodeAnswer,
  encodeControl,
  encodeOffer,
  errorFrame,
  isValidId,
  MAX_ALIAS_LENGTH,
  MAX_BINARY_FRAME,
  MAX_CAP_LENGTH,
  MAX_CAPS,
  MAX_DEVICE_TYPE_LENGTH,
  MAX_FILES,
  MAX_OFFER_BYTES,
  MAX_PLATFORM_LENGTH,
  MAX_REASON_LENGTH,
  MAX_TEXT_BYTES,
  NONCE_LENGTH,
  parseControl,
  PROGRESS_STEP,
  RECV_WINDOW,
  RtcError,
  validateFiles,
  type AnswerMsg,
  type AuthMsg,
  type CancelMsg,
  type ControlMessage,
  type DeviceInfo,
  type DoneMsg,
  type ErrorMsg,
  type FileAckMsg,
  type FileEndMsg,
  type FileMeta,
  type FileMsg,
  type HelloMsg,
  type OfferMsg,
  type ProgressMsg,
} from "./protocol";
import { authPayload, deriveRoomKey, roomMac, transcriptHash, verifyRoomMac, type Role } from "./transcript";

// ── Public types ──────────────────────────────────────────────────────────

/** The subset of RTCDataChannel the session uses (a real RTCDataChannel satisfies it). */
export interface DataChannelLike {
  readonly readyState: RTCDataChannelState;
  readonly bufferedAmount: number;
  bufferedAmountLowThreshold: number;
  binaryType: BinaryType;
  send(data: string): void;
  send(data: ArrayBufferView<ArrayBuffer>): void;
  close(): void;
  onopen: ((ev: Event) => unknown) | null;
  onclose: ((ev: Event) => unknown) | null;
  onmessage: ((ev: MessageEvent) => unknown) | null;
  onbufferedamountlow: ((ev: Event) => unknown) | null;
}

/** A file to send. `slice` returns exactly `end - start` bytes (e.g. `blob.slice(a, b).arrayBuffer()`). */
export interface SourceFile extends FileMeta {
  slice(start: number, end: number): Promise<ArrayBuffer>;
}

export interface TransferRequest {
  /**
   * 1 to 256 printable ASCII characters (e.g. `randomId()`), unique within a
   * session. Offer the same id again in a *new* session to resume.
   */
  transferId: string;
  files: SourceFile[];
  /** A message (or link) shown to the receiver; at most `MAX_TEXT_BYTES` as a JSON string. */
  text?: string;
}

export interface TransferOutcome {
  transferId: string;
  declined: boolean;
  /** Accepted files the receiver verified and committed. */
  completed: string[];
  /** Accepted files that failed (hash mismatch, write error, …). */
  failed: string[];
  /** Offered files the receiver did not accept (all of them when declined). */
  skipped: string[];
}

export interface SinkContext {
  transferId: string;
  /** The verified peer's public key (base64url): a stable key for partial files. */
  peerKey: string;
}

/**
 * Why a writer is aborted. Sinks should keep partial data for "closed" and
 * "timeout" (resumable) and may discard it for the others.
 */
export type SinkAbortReason = "cancelled" | "closed" | "timeout" | "integrity" | "overrun" | "error";

export interface SinkWriter {
  write(chunk: Uint8Array): Promise<void>;
  /** Commits the file. Only called after size and SHA-256 were verified. */
  close(): Promise<void>;
  abort(reason: SinkAbortReason): Promise<void>;
}

export interface FileSink {
  /** Opens a writer positioned at `offset` (the sink truncates anything beyond it). */
  open(file: FileMeta, offset: number, context: SinkContext): Promise<SinkWriter>;
  /**
   * Yields the first `offset` bytes already stored for `file`, used to hash the
   * prefix of a resumed file. Required to accept offsets > 0.
   */
  readPrefix?(file: FileMeta, offset: number, context: SinkContext): AsyncIterable<Uint8Array>;
}

export interface RemotePeer {
  alg: IdentityAlg;
  /** Raw public key, base64url. Verified: the peer proved possession of it. */
  key: string;
  device: DeviceInfo;
  caps: string[];
  /** Handshake transcript hash T. */
  transcript: Bytes;
  /** 6-digit first-contact verification code derived from T. */
  shortCode: string;
  /** True when a room MAC was required and verified. */
  roomVerified: boolean;
}

export type SessionState = "connecting" | "handshake" | "ready" | "closed";
export type Direction = "send" | "receive";
export type CloseReason = "local" | "remote" | "timeout" | "auth" | "protocol" | "error";

export interface IncomingOffer {
  readonly transferId: string;
  readonly files: readonly FileMeta[];
  readonly text?: string;
  readonly peer: RemotePeer;
  /** When the offer is cancelled if nobody decides (ms since the epoch); null = never. */
  readonly expiresAt: number | null;
  /**
   * Accepts `ids` (default: all), resuming each from `offsets[id]` (default 0).
   * Throws `RtcError` ("invalid-state", "invalid", "no-sink", "no-resume").
   */
  accept(ids?: readonly string[], offsets?: Readonly<Record<string, number>>): void;
  decline(): void;
}

export interface TransferAccepted {
  transferId: string;
  /** Accepted file ids, in offer order. */
  files: string[];
  /** Resume offsets the receiver asked for (ids without an entry start at 0). */
  offsets: Record<string, number>;
}

export interface TransferProgress {
  transferId: string;
  direction: Direction;
  fileId: string;
  /** Bytes of this file done so far (including a resumed prefix). */
  bytes: number;
  totalBytes: number;
}

export interface FileComplete {
  transferId: string;
  direction: Direction;
  fileId: string;
  ok: boolean;
  sha256?: string;
  error?: string;
}

export interface TransferDone {
  transferId: string;
  direction: Direction;
  completed: string[];
  failed: string[];
  skipped: string[];
}

export interface TransferCancelled {
  transferId: string;
  direction: Direction;
  reason?: string;
  /** The peer cancelled (or closed the session). */
  byRemote: boolean;
  /**
   * The session closed under the transfer (connection lost, timeout, protocol
   * error) instead of an explicit cancel. Partial files were kept by the sink
   * ("closed"/"timeout"); offer the same transferId in a new session to resume.
   */
  interrupted: boolean;
}

export interface SessionError {
  code: string;
  message: string;
  /** True when the peer reported the error. */
  remote: boolean;
}

export interface SessionClosed {
  reason: CloseReason;
  message?: string;
}

/**
 * Events. Per transfer, the sender sees `accepted` → `progress`* →
 * `fileComplete`* → `done`, or `cancelled`; a decline only resolves the
 * `sendTransfer` promise. The receiver sees `offer` → `progress`* →
 * `fileComplete`* → `done`, or `cancelled` (also on decision timeout).
 */
export interface SessionEvents {
  ready: RemotePeer;
  offer: IncomingOffer;
  accepted: TransferAccepted;
  progress: TransferProgress;
  fileComplete: FileComplete;
  done: TransferDone;
  cancelled: TransferCancelled;
  error: SessionError;
  closed: SessionClosed;
}

export interface PeerSessionOptions {
  channel: DataChannelLike;
  role: Role;
  /** Signaling session id (bound into the transcript). */
  sessionId: string;
  /** Normalized DTLS fingerprints from the local / remote SDP (`extractFingerprint`). */
  localFingerprint: string;
  remoteFingerprint: string;
  identity: Identity;
  device: DeviceInfo;
  caps?: string[];
  /** Secret of a link/QR room: both sides must prove it (HMAC over T). */
  roomSecret?: Uint8Array;
  /** Pin the peer's public key (base64url), e.g. for a known trusted device. */
  expectedPeerKey?: string;
  /** Remote SDP `a=max-message-size` (0 or Infinity = unlimited). Unknown → 16 KiB chunks. */
  maxMessageSize?: number;
  /** Where received files go. Without a sink only text-only offers can be accepted. */
  sink?: FileSink;
  /** `ping` interval (default 10 s). */
  pingIntervalMs?: number;
  /** Close when nothing arrived from the peer for this long (default 30 s). */
  timeoutMs?: number;
  /** Close when the handshake is not done this long after the channel opened (default `timeoutMs`). */
  handshakeTimeoutMs?: number;
  /** Cancel incoming offers nobody decided on within this time (default 300 s; 0 = never). */
  decisionTimeoutMs?: number;
  /** Minimum interval between `progress` events per direction (default 100 ms). */
  progressIntervalMs?: number;
}

// ── Internals ─────────────────────────────────────────────────────────────

const READ_BLOCK = 1024 * 1024;
const DRAIN_POLL_MS = 250;
/** Control frames waiting in the inbox (UTF-16 units); compliant peers stay far below. */
const MAX_QUEUED_CONTROL = 16 * 1024 * 1024;

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
  reject(reason: unknown): void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  promise.catch(() => {});
  return { promise, resolve, reject };
}

const noop = () => {};

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function protocolError(message: string): RtcError {
  return new RtcError("protocol", message);
}

/** A copy that keeps ids like "__proto__" as plain keys. */
function copyOffsets(offsets: Readonly<Record<string, number>>): Record<string, number> {
  return Object.assign(Object.create(null) as Record<string, number>, offsets);
}

interface SendSlot {
  /** Set when the request was cancelled before it started (or the session closed). */
  cancelled: RtcError | null;
  /** Rejects the caller's promise right away, even while the request is still queued. */
  abort: Deferred<never>;
}

interface Outgoing {
  transferId: string;
  files: Map<string, SourceFile>;
  answer: Deferred<AnswerMsg>;
  /** Accumulates a (possibly split) answer. */
  accepted: Set<string>;
  offsets: Record<string, number>;
  answered: boolean;
  acks: Map<string, Deferred<FileAckMsg>>;
  /** Files whose `file-end` was sent (acks for other files are a protocol error). */
  ended: Set<string>;
  abort: Deferred<never>;
  aborted: RtcError | null;
  /** File bytes sent in this transfer, and the receiver's last `progress` report. */
  sent: number;
  acked: number;
}

interface IncomingFile {
  meta: FileMeta;
  received: number;
  hasher: ReturnType<typeof sha256.create>;
  writer: SinkWriter | null;
  error: string | null;
}

interface Incoming {
  transferId: string;
  files: Map<string, FileMeta>;
  /** Offered files, in offer order. */
  metas: FileMeta[];
  text?: string;
  /** Size of the offer frames received so far, and the summed file sizes. */
  offerBytes: number;
  totalSize: number;
  /** Every offer frame arrived. */
  complete: boolean;
  /** The `offer` event was emitted. */
  announced: boolean;
  timer: ReturnType<typeof setTimeout> | null;
  answered: boolean;
  accepted: Map<string, number>;
  finished: Set<string>;
  completed: string[];
  failed: string[];
  current: IncomingFile | null;
  cancelled: boolean;
  /** What a writer opened after the transfer ended is aborted with. */
  abortReason: SinkAbortReason;
  /** File bytes processed in this transfer, and the count last reported to the sender. */
  processed: number;
  reported: number;
}

export class PeerSession extends Emitter<SessionEvents> {
  readonly role: Role;
  readonly sessionId: string;
  /** Resolves once the peer is authenticated; rejects when the handshake fails or the session closes first. */
  readonly ready: Promise<RemotePeer>;
  /** Binary frame size used when sending. */
  readonly chunkSize: number;

  private readonly ch: DataChannelLike;
  private readonly opts: PeerSessionOptions;
  private readonly device: DeviceInfo;
  private readonly caps: string[];
  private readonly roomKey: Bytes | null;
  private readonly nonce = randomBytes(NONCE_LENGTH);
  private readonly pingIntervalMs: number;
  private readonly timeoutMs: number;
  private readonly handshakeTimeoutMs: number;
  private readonly decisionTimeoutMs: number;
  private readonly progressIntervalMs: number;
  private readonly readyGate = deferred<RemotePeer>();

  private _state: SessionState = "connecting";
  private _peer: RemotePeer | null = null;
  private localKey: Bytes | null = null;
  private remoteHello: HelloMsg | null = null;
  private transcript: Bytes | null = null;
  private inbox: Promise<void> = Promise.resolve();
  /** Binary bytes / control-frame size received but not processed yet. */
  private queuedBinary = 0;
  private queuedControl = 0;
  private lastInbound = Date.now();
  private timers: ReturnType<typeof setInterval>[] = [];
  private handshakeTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly waiters = new Set<() => void>();
  private sendChain: Promise<unknown> = Promise.resolve();
  /** Outgoing requests, queued or running, by transfer id. */
  private readonly sends = new Map<string, SendSlot>();
  /** Every outgoing transfer id used in this session (stale replies must stay unambiguous). */
  private readonly usedIds = new Set<string>();
  private out: Outgoing | null = null;
  private inc: Incoming | null = null;
  /** Incoming transfer whose trailing frames are dropped after it was cancelled. */
  private discard: string | null = null;
  private readonly lastProgress: Record<Direction, number> = { send: 0, receive: 0 };

  constructor(options: PeerSessionOptions) {
    super();
    this.opts = options;
    this.ch = options.channel;
    this.role = options.role;
    this.sessionId = options.sessionId;
    const d = options.device;
    this.device = {
      alias: d.alias.slice(0, MAX_ALIAS_LENGTH),
      deviceType: d.deviceType.slice(0, MAX_DEVICE_TYPE_LENGTH),
      platform: d.platform.slice(0, MAX_PLATFORM_LENGTH),
    };
    this.caps = (options.caps ?? [...DC_CAPS]).slice(0, MAX_CAPS).map((c) => c.slice(0, MAX_CAP_LENGTH));
    this.roomKey = options.roomSecret ? deriveRoomKey(options.roomSecret) : null;
    this.chunkSize = chunkSizeFor(options.maxMessageSize);
    this.pingIntervalMs = options.pingIntervalMs ?? 10_000;
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs ?? this.timeoutMs;
    this.decisionTimeoutMs = options.decisionTimeoutMs ?? 300_000;
    this.progressIntervalMs = options.progressIntervalMs ?? 100;
    this.ready = this.readyGate.promise;

    const ch = this.ch;
    ch.binaryType = "arraybuffer";
    ch.bufferedAmountLowThreshold = BUFFER_LOW_WATER;
    ch.onopen = () => this.onOpen();
    ch.onmessage = (ev) => this.onFrame(ev.data);
    ch.onclose = () => this.shutdown("remote", "data channel closed");
    ch.onbufferedamountlow = () => this.wake();
    if (ch.readyState === "open") this.onOpen();
    else if (ch.readyState !== "connecting") queueMicrotask(() => this.shutdown("remote", "data channel closed"));
  }

  get state(): SessionState {
    return this._state;
  }

  /** The authenticated peer, once `ready`. */
  get peer(): RemotePeer | null {
    return this._peer;
  }

  /**
   * Offers files (and/or text) to the peer and streams the accepted ones.
   * Calls are queued: one outgoing transfer at a time. Resolves when the peer
   * acknowledged every accepted file (or declined); rejects with an
   * `RtcError` when the transfer is cancelled ("cancelled"), the request is
   * invalid ("invalid", "too-large"), a source cannot be read ("source") or
   * the session closes ("closed", "timeout", "auth", "protocol").
   */
  sendTransfer(request: TransferRequest): Promise<TransferOutcome> {
    const id = request.transferId;
    if (!isValidId(id)) return Promise.reject(new RtcError("invalid", "invalid transferId"));
    if (this.usedIds.has(id)) {
      return Promise.reject(new RtcError("invalid", `transferId ${JSON.stringify(id)} was already used in this session`));
    }
    this.usedIds.add(id);
    const slot: SendSlot = { cancelled: null, abort: deferred() };
    this.sends.set(id, slot);
    const run = this.sendChain.then(() => this.runTransfer(request, slot));
    this.sendChain = run.catch(noop);
    return Promise.race([run, slot.abort.promise]).finally(() => {
      if (this.sends.get(id) === slot) this.sends.delete(id);
    });
  }

  /** Cancels the given transfer (running, queued or incoming), or every transfer in both directions. */
  cancel(reason?: string, transferId?: string): void {
    const matches = (id: string) => transferId === undefined || id === transferId;
    const out = this.out;
    if (out && matches(out.transferId)) {
      this.abortOutgoing(out, new RtcError("cancelled", reason ?? "cancelled"), false, reason);
    }
    for (const [id, slot] of [...this.sends]) {
      if (slot.cancelled || id === out?.transferId || !matches(id)) continue;
      slot.cancelled = new RtcError("cancelled", reason ?? "cancelled");
      slot.abort.reject(slot.cancelled);
      this.emit("cancelled", cancelledEvent(id, "send", false, false, reason));
    }
    const inc = this.inc;
    if (inc && matches(inc.transferId)) this.abortIncoming(inc, false, reason);
  }

  close(message?: string): void {
    this.shutdown("local", message);
  }

  // ── Channel plumbing ────────────────────────────────────────────────────

  private onOpen(): void {
    if (this._state !== "connecting") return;
    this._state = "handshake";
    this.lastInbound = Date.now();
    this.timers.push(
      setInterval(() => this.trySend({ t: "ping" }), this.pingIntervalMs),
      setInterval(
        () => {
          if (Date.now() - this.lastInbound > this.timeoutMs) this.shutdown("timeout", "no frames from the peer");
        },
        Math.max(10, Math.min(1000, this.timeoutMs / 4)),
      ),
    );
    this.handshakeTimer = setTimeout(() => {
      if (this._state === "handshake") this.shutdown("timeout", "handshake timed out");
    }, this.handshakeTimeoutMs);
    this.enqueue(() => this.sendHello());
  }

  /** Runs `task` after every earlier inbound frame; `settle` runs afterwards in any case. */
  private enqueue(task: () => unknown, settle?: () => void): void {
    this.inbox = this.inbox
      .then(async () => {
        try {
          if (this._state !== "closed") await task();
        } finally {
          settle?.();
        }
      })
      .catch((err) => this.fail(err));
  }

  private onFrame(data: unknown): void {
    if (this._state === "closed") return;
    if (this._state === "connecting") this.onOpen(); // a frame implies the channel is open
    this.lastInbound = Date.now();
    if (typeof data === "string") {
      this.onText(data);
      return;
    }
    let bytes: Bytes | null = null;
    if (isArrayBuffer(data)) bytes = new Uint8Array(data);
    else if (ArrayBuffer.isView(data)) bytes = new Uint8Array(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
    if (!bytes) {
      this.fail(protocolError("unsupported frame type"));
      return;
    }
    if (bytes.byteLength > MAX_BINARY_FRAME) {
      this.fail(protocolError("binary frame too large"));
      return;
    }
    // A compliant sender never has more than RECV_WINDOW unreported bytes in
    // flight, so this bounds the receive queue (one frame of slack).
    const size = bytes.byteLength;
    this.queuedBinary += size;
    if (this.queuedBinary > RECV_WINDOW + MAX_BINARY_FRAME) {
      this.fail(protocolError("the peer ignored the flow-control window"));
      return;
    }
    const chunk = bytes;
    this.enqueue(
      () => this.onBinary(chunk),
      () => {
        this.queuedBinary -= size;
      },
    );
  }

  private onText(data: string): void {
    let msg: ControlMessage;
    try {
      msg = parseControl(data);
    } catch (err) {
      this.fail(err);
      return;
    }
    // Liveness, cancellation and sender-side replies bypass the inbox so a
    // slow sink can neither starve pongs nor delay a cancel.
    try {
      switch (msg.t) {
        case "ping":
          this.trySend({ t: "pong" });
          return;
        case "pong":
          return;
        case "cancel":
          if (this.onRemoteCancel(msg)) return;
          break; // possibly for an offer still waiting in the inbox: handle it in order
        case "answer":
          this.onAnswer(msg);
          return;
        case "file-ack":
          this.onFileAck(msg);
          return;
        case "progress":
          this.onProgress(msg);
          return;
        case "error":
          this.onRemoteError(msg);
          return;
      }
    } catch (err) {
      this.fail(err);
      return;
    }
    const cost = data.length;
    this.queuedControl += cost;
    if (this.queuedControl > MAX_QUEUED_CONTROL) {
      this.fail(protocolError("too many queued control messages"));
      return;
    }
    this.enqueue(
      () => this.handleControl(msg, cost),
      () => {
        this.queuedControl -= cost;
      },
    );
  }

  private sendControl(msg: ControlMessage): void {
    this.sendRaw(encodeControl(msg));
  }

  private sendRaw(text: string): void {
    if (this.ch.readyState !== "open") throw new RtcError("closed", "data channel is not open");
    this.ch.send(text);
  }

  private trySend(msg: ControlMessage): void {
    try {
      this.sendControl(msg);
    } catch {
      // Channel already gone; the close path reports it.
    }
  }

  private fail(err: unknown): void {
    if (this._state === "closed") return;
    const e = err instanceof RtcError ? err : new RtcError("internal", errorMessage(err));
    this.trySend(errorFrame(e.code, e.message));
    this.emit("error", { code: e.code, message: e.message, remote: false });
    const reason: CloseReason =
      e.code === "auth" ? "auth" : e.code === "protocol" || e.code === "overrun" || e.code === "too-large" ? "protocol" : "error";
    this.shutdown(reason, e.message);
  }

  private shutdown(reason: CloseReason, message?: string): void {
    if (this._state === "closed") return;
    this._state = "closed";
    for (const t of this.timers) clearInterval(t);
    this.timers = [];
    if (this.handshakeTimer !== null) clearTimeout(this.handshakeTimer);
    this.handshakeTimer = null;
    const text = message ?? `session closed (${reason})`;
    const code = reason === "timeout" || reason === "auth" || reason === "protocol" ? reason : "closed";
    const err = new RtcError(code, text);
    this.readyGate.reject(err);
    const byRemote = reason === "remote";
    const out = this.out;
    if (out && !out.aborted) {
      out.aborted = err;
      out.abort.reject(err);
      this.emit("cancelled", cancelledEvent(out.transferId, "send", byRemote, true, text));
    }
    for (const [id, slot] of this.sends) {
      slot.cancelled ??= err;
      if (id !== out?.transferId) slot.abort.reject(slot.cancelled); // the running one rejects through `out`
    }
    const inc = this.inc;
    if (inc) {
      inc.cancelled = true;
      inc.abortReason = reason === "timeout" ? "timeout" : "closed";
      this.clearDecisionTimer(inc);
      this.inc = null;
      const writer = this.takeWriter(inc);
      void writer?.abort(inc.abortReason).catch(noop);
      if (inc.announced) this.emit("cancelled", cancelledEvent(inc.transferId, "receive", byRemote, true, text));
    }
    this.wake();
    try {
      this.ch.close();
    } catch {
      // ignore
    }
    this.emit("closed", message === undefined ? { reason } : { reason, message });
  }

  private wake(): void {
    for (const wake of [...this.waiters]) wake();
  }

  /** Resolves on the next wake-up (`bufferedamountlow`, a `progress` report, a cancel) or after a short poll. */
  private nextWake(): Promise<void> {
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        this.waiters.delete(done);
        resolve();
      };
      const timer = setTimeout(done, DRAIN_POLL_MS);
      this.waiters.add(done);
    });
  }

  /** Throttled per direction; the final event of a file is always emitted. */
  private progress(direction: Direction, transferId: string, fileId: string, bytes: number, totalBytes: number): void {
    const now = Date.now();
    if (bytes < totalBytes && now - this.lastProgress[direction] < this.progressIntervalMs) return;
    this.lastProgress[direction] = now;
    this.emit("progress", { transferId, direction, fileId, bytes, totalBytes });
  }

  // ── Handshake ───────────────────────────────────────────────────────────

  private async sendHello(): Promise<void> {
    const key = await exportPublicKey(this.opts.identity);
    this.localKey = b64urlDecode(key);
    const hello: HelloMsg = {
      t: "hello",
      v: DC_VERSION,
      alg: this.opts.identity.alg,
      key,
      nonce: b64urlEncode(this.nonce),
      device: this.device,
      caps: this.caps,
    };
    this.sendControl(hello);
  }

  private async handleControl(msg: ControlMessage, cost: number): Promise<void> {
    if (this._state === "handshake") {
      if (msg.t === "hello") return this.onHello(msg);
      if (msg.t === "auth") return this.onAuth(msg);
      throw protocolError(`unexpected "${msg.t}" before authentication`);
    }
    switch (msg.t) {
      case "offer":
        return this.onOffer(msg, cost);
      case "file":
        return this.onFileStart(msg);
      case "file-end":
        return this.onFileEnd(msg);
      case "done":
        return this.onDone(msg);
      case "cancel":
        this.onRemoteCancel(msg); // stale when it still names nothing
        return;
      default:
        throw protocolError(`unexpected "${msg.t}" message`);
    }
  }

  private async onHello(msg: HelloMsg): Promise<void> {
    if (this.remoteHello) throw protocolError("duplicate hello");
    const localKey = this.localKey;
    if (!localKey) throw new RtcError("internal", "local hello not sent");
    this.remoteHello = msg;
    const remoteNonce = b64urlDecode(msg.nonce);
    if (equalBytes(remoteNonce, this.nonce)) throw new RtcError("auth", "reflected handshake");
    if (this.opts.expectedPeerKey !== undefined && msg.key !== this.opts.expectedPeerKey) {
      throw new RtcError("auth", "unexpected peer key");
    }
    const remoteKey = b64urlDecode(msg.key);
    const offerer = this.role === "offerer";
    const t = transcriptHash({
      sessionId: this.sessionId,
      fpOfferer: offerer ? this.opts.localFingerprint : this.opts.remoteFingerprint,
      fpAnswerer: offerer ? this.opts.remoteFingerprint : this.opts.localFingerprint,
      nonceOfferer: offerer ? this.nonce : remoteNonce,
      nonceAnswerer: offerer ? remoteNonce : this.nonce,
      keyOfferer: offerer ? localKey : remoteKey,
      keyAnswerer: offerer ? remoteKey : localKey,
    });
    this.transcript = t;
    const auth: AuthMsg = { t: "auth", sig: await sign(this.opts.identity, authPayload(this.role, t)) };
    if (this.roomKey) auth.mac = roomMac(this.roomKey, t);
    this.sendControl(auth);
  }

  private async onAuth(msg: AuthMsg): Promise<void> {
    const hello = this.remoteHello;
    const t = this.transcript;
    if (!hello || !t) throw protocolError("auth before hello");
    const remoteRole: Role = this.role === "offerer" ? "answerer" : "offerer";
    let ok = await verify(hello.alg, hello.key, authPayload(remoteRole, t), msg.sig);
    if (ok && this.roomKey) ok = msg.mac !== undefined && verifyRoomMac(this.roomKey, t, msg.mac);
    if (!ok) throw new RtcError("auth", "peer authentication failed");
    if (this._state !== "handshake") return;
    this._state = "ready";
    if (this.handshakeTimer !== null) clearTimeout(this.handshakeTimer);
    this.handshakeTimer = null;
    const peer: RemotePeer = {
      alg: hello.alg,
      key: hello.key,
      device: hello.device,
      caps: hello.caps,
      transcript: t,
      shortCode: shortCode(t),
      roomVerified: this.roomKey !== null,
    };
    this._peer = peer;
    this.readyGate.resolve(peer);
    this.emit("ready", peer);
  }

  private onRemoteError(msg: ErrorMsg): void {
    this.emit("error", { code: msg.code, message: msg.message, remote: true });
    this.shutdown(msg.code === "auth" ? "auth" : "remote", msg.message);
  }

  // ── Sending ─────────────────────────────────────────────────────────────

  private async runTransfer(request: TransferRequest, slot: SendSlot): Promise<TransferOutcome> {
    await this.ready;
    if (slot.cancelled) throw slot.cancelled;
    if (this._state !== "ready") throw new RtcError("closed", "session closed");
    let metas: FileMeta[];
    try {
      metas = validateFiles(request.files);
    } catch (err) {
      throw new RtcError("invalid", errorMessage(err));
    }
    if (request.text !== undefined) {
      if (typeof request.text !== "string") throw new RtcError("invalid", "text must be a string");
      if (utf8Length(JSON.stringify(request.text)) > MAX_TEXT_BYTES) throw new RtcError("too-large", `text exceeds ${MAX_TEXT_BYTES} bytes`);
    }
    const transferId = request.transferId;
    const offer: OfferMsg = { t: "offer", transferId, files: metas };
    if (request.text !== undefined) offer.text = request.text;
    const frames = encodeOffer(offer); // throws RtcError("too-large")

    const out: Outgoing = {
      transferId,
      files: new Map(request.files.map((f) => [f.id, f])),
      answer: deferred(),
      accepted: new Set(),
      offsets: Object.create(null) as Record<string, number>,
      answered: false,
      acks: new Map(),
      ended: new Set(),
      abort: deferred(),
      aborted: null,
      sent: 0,
      acked: 0,
    };
    this.out = out;
    try {
      for (const frame of frames) this.sendRaw(frame);
      const answer = await this.race(out, out.answer.promise);
      const order = metas.map((m) => m.id);
      if (answer.declined) return { transferId, declined: true, completed: [], failed: [], skipped: order };

      const accepted = order.filter((id) => out.accepted.has(id));
      const skipped = order.filter((id) => !out.accepted.has(id));
      this.emit("accepted", { transferId, files: accepted, offsets: copyOffsets(answer.offsets) });
      const completed: string[] = [];
      const failed: string[] = [];
      const acked: Promise<void>[] = [];
      for (const id of accepted) {
        const offset = Object.hasOwn(answer.offsets, id) ? answer.offsets[id]! : 0;
        const ack = deferred<FileAckMsg>();
        out.acks.set(id, ack);
        const digest = await this.streamFile(out, out.files.get(id)!, offset);
        acked.push(
          ack.promise.then((msg) => {
            if (out.aborted) return;
            const ok = msg.ok && (msg.sha256 === undefined || msg.sha256 === digest);
            (ok ? completed : failed).push(id);
            const ev: FileComplete = { transferId, direction: "send", fileId: id, ok, sha256: digest };
            if (!ok) ev.error = msg.ok ? "the receiver computed a different SHA-256" : (msg.error ?? "rejected by the receiver");
            this.emit("fileComplete", ev);
          }),
        );
      }
      await this.race(out, Promise.all(acked));
      this.checkOutgoing(out);
      const done: DoneMsg = { t: "done", transferId };
      this.sendControl(done);
      const index = new Map(order.map((id, i) => [id, i]));
      const byOrder = (a: string, b: string) => index.get(a)! - index.get(b)!;
      completed.sort(byOrder);
      failed.sort(byOrder);
      this.emit("done", { transferId, direction: "send", completed, failed, skipped });
      return { transferId, declined: false, completed, failed, skipped };
    } catch (err) {
      if (!out.aborted && this._state === "ready") {
        if (this.ch.readyState !== "open") {
          this.shutdown("remote", "data channel closed");
        } else {
          // A local failure (e.g. the source became unreadable): tell the peer.
          const e = err instanceof RtcError ? err : new RtcError("source", errorMessage(err));
          this.abortOutgoing(out, e, false, e.message);
        }
      }
      throw out.aborted ?? err;
    } finally {
      if (this.out === out) this.out = null;
    }
  }

  /** Streams one file from `offset`; returns the full-file SHA-256 (lowercase hex). */
  private async streamFile(out: Outgoing, file: SourceFile, offset: number): Promise<string> {
    const { id, size } = file;
    const start: FileMsg = { t: "file", id, offset };
    this.sendControl(start);
    const hasher = sha256.create();
    // Resume: hash the prefix the receiver already has (the receiver does the same in parallel).
    for (let pos = 0; pos < offset; ) {
      const end = Math.min(offset, pos + READ_BLOCK);
      hasher.update(await this.race(out, this.read(file, pos, end)));
      pos = end;
    }
    let pos = offset;
    let next = pos < size ? this.read(file, pos, Math.min(size, pos + READ_BLOCK)) : null;
    while (next) {
      const block = await this.race(out, next);
      const blockEnd = pos + block.byteLength;
      next = blockEnd < size ? this.read(file, blockEnd, Math.min(size, blockEnd + READ_BLOCK)) : null;
      for (let o = 0; o < block.byteLength; o += this.chunkSize) {
        const chunk = block.subarray(o, Math.min(block.byteLength, o + this.chunkSize));
        await this.clearToSend(out, chunk.byteLength);
        hasher.update(chunk);
        this.ch.send(chunk);
        out.sent += chunk.byteLength;
        pos += chunk.byteLength;
        this.progress("send", out.transferId, id, pos, size);
      }
    }
    const digest = toHex(hasher.digest());
    const end: FileEndMsg = { t: "file-end", id, sha256: digest };
    this.sendControl(end);
    out.ended.add(id);
    return digest;
  }

  /** Waits until `size` more bytes may be sent: channel buffer drained below the mark and flow-control window open. */
  private async clearToSend(out: Outgoing, size: number): Promise<void> {
    if (this.ch.bufferedAmount > BUFFER_HIGH_WATER) {
      while (this.ch.bufferedAmount > BUFFER_LOW_WATER) {
        this.checkOutgoing(out);
        await this.race(out, this.nextWake());
      }
    }
    while (out.sent + size - out.acked > RECV_WINDOW) {
      this.checkOutgoing(out);
      await this.race(out, this.nextWake());
    }
    this.checkOutgoing(out);
  }

  private read(file: SourceFile, start: number, end: number): Promise<Bytes> {
    const where = `${JSON.stringify(file.id)} [${start}, ${end})`;
    const p = Promise.resolve()
      .then(() => file.slice(start, end))
      .then(
        (buf) => {
          if (!isArrayBuffer(buf) || buf.byteLength !== end - start) throw new RtcError("source", `could not read ${where}`);
          return new Uint8Array(buf);
        },
        (err: unknown) => {
          throw new RtcError("source", `could not read ${where}: ${errorMessage(err)}`);
        },
      );
    p.catch(noop); // a prefetch may be abandoned when the transfer stops
    return p;
  }

  private race<T>(out: Outgoing, p: Promise<T>): Promise<T> {
    return Promise.race([p, out.abort.promise]);
  }

  private checkOutgoing(out: Outgoing): void {
    if (out.aborted) throw out.aborted;
    if (this._state !== "ready") throw new RtcError("closed", "session closed");
    if (this.ch.readyState !== "open") throw new RtcError("closed", "data channel is not open");
  }

  private onAnswer(msg: AnswerMsg): void {
    if (this._state !== "ready") throw protocolError("answer before authentication");
    const out = this.out;
    // Answers for other transfers are stale (sent before our cancel arrived).
    if (!out || out.transferId !== msg.transferId || out.answered || out.aborted) return;
    if (msg.declined) {
      if (out.accepted.size > 0) throw protocolError("declining answer after accepting answer frames");
      out.answered = true;
      out.answer.resolve(msg);
      return;
    }
    for (const id of msg.accept) {
      const file = out.files.get(id);
      if (!file) throw protocolError(`answer accepts unknown file ${JSON.stringify(id)}`);
      if (out.accepted.has(id)) throw protocolError(`answer accepts ${JSON.stringify(id)} twice`);
      out.accepted.add(id);
      const offset = Object.hasOwn(msg.offsets, id) ? msg.offsets[id]! : 0;
      if (offset > file.size) throw protocolError(`offset beyond the end of ${JSON.stringify(id)}`);
      if (offset > 0) out.offsets[id] = offset;
    }
    if (msg.more) return;
    out.answered = true;
    out.answer.resolve({ t: "answer", transferId: out.transferId, accept: [...out.accepted], offsets: out.offsets });
  }

  private onFileAck(msg: FileAckMsg): void {
    if (this._state !== "ready") throw protocolError("file-ack before authentication");
    const out = this.out;
    const ack = out?.acks.get(msg.id);
    if (!out || !ack) return; // stale ack from a cancelled transfer
    if (!out.ended.has(msg.id)) throw protocolError(`file-ack for ${JSON.stringify(msg.id)} before its file-end`);
    out.acks.delete(msg.id);
    ack.resolve(msg);
  }

  private onProgress(msg: ProgressMsg): void {
    if (this._state !== "ready") throw protocolError("progress before authentication");
    const out = this.out;
    if (!out || out.transferId !== msg.transferId || out.aborted) return;
    if (msg.bytes > out.sent) throw protocolError("progress beyond the bytes sent");
    if (msg.bytes > out.acked) {
      out.acked = msg.bytes;
      this.wake();
    }
  }

  private abortOutgoing(out: Outgoing, err: RtcError, byRemote: boolean, reason?: string): void {
    if (out.aborted) return;
    out.aborted = err;
    out.abort.reject(err);
    if (!byRemote) {
      const cancel: CancelMsg = { t: "cancel", transferId: out.transferId };
      if (reason !== undefined) cancel.reason = reason.slice(0, MAX_REASON_LENGTH);
      this.trySend(cancel);
    }
    this.wake();
    this.emit("cancelled", cancelledEvent(out.transferId, "send", byRemote, false, reason));
  }

  // ── Receiving ───────────────────────────────────────────────────────────

  private onOffer(msg: OfferMsg, cost: number): void {
    let inc = this.inc;
    if (inc && !inc.complete && inc.transferId === msg.transferId) {
      if (msg.text !== undefined) throw protocolError("text in an offer continuation");
    } else {
      if (inc) throw protocolError("offer while another incoming transfer is active");
      this.discard = null;
      inc = {
        transferId: msg.transferId,
        files: new Map(),
        metas: [],
        offerBytes: 0,
        totalSize: 0,
        complete: false,
        announced: false,
        timer: null,
        answered: false,
        accepted: new Map(),
        finished: new Set(),
        completed: [],
        failed: [],
        current: null,
        cancelled: false,
        abortReason: "cancelled",
        processed: 0,
        reported: 0,
      };
      if (msg.text !== undefined) inc.text = msg.text;
      this.inc = inc;
    }
    inc.offerBytes += cost;
    if (inc.offerBytes > MAX_OFFER_BYTES) throw protocolError("offer too large");
    if (inc.metas.length + msg.files.length > MAX_FILES) throw protocolError(`too many files (max ${MAX_FILES})`);
    for (const f of msg.files) {
      if (inc.files.has(f.id)) throw protocolError(`duplicate file id ${JSON.stringify(f.id)}`);
      inc.totalSize += f.size;
      if (!Number.isSafeInteger(inc.totalSize)) throw protocolError("total size too large");
      inc.files.set(f.id, f);
      inc.metas.push(f);
    }
    if (msg.more) return;
    inc.complete = true;
    this.announce(inc);
  }

  private announce(inc: Incoming): void {
    const ms = this.decisionTimeoutMs;
    const offer: IncomingOffer = {
      transferId: inc.transferId,
      files: inc.metas,
      text: inc.text,
      peer: this._peer!,
      expiresAt: ms > 0 ? Date.now() + ms : null,
      accept: (ids, offsets) => this.acceptOffer(inc, ids, offsets),
      decline: () => this.declineOffer(inc),
    };
    if (this.listenerCount("offer") === 0) {
      this.declineOffer(inc); // nobody can decide: never auto-accept
      return;
    }
    inc.announced = true;
    if (ms > 0) inc.timer = setTimeout(() => this.expireOffer(inc), ms);
    this.emit("offer", offer);
  }

  private expireOffer(inc: Incoming): void {
    inc.timer = null;
    if (this.inc !== inc || inc.answered || inc.cancelled || this._state !== "ready") return;
    this.abortIncoming(inc, false, "timeout");
  }

  private pending(inc: Incoming): void {
    if (this.inc !== inc || inc.answered || inc.cancelled || this._state !== "ready") {
      throw new RtcError("invalid-state", "the offer is no longer pending");
    }
  }

  private acceptOffer(inc: Incoming, ids?: readonly string[], offsets?: Readonly<Record<string, number>>): void {
    this.pending(inc);
    const list = ids ?? inc.metas.map((m) => m.id);
    const accepted = new Map<string, number>();
    const wire = Object.create(null) as Record<string, number>;
    let resumes = false;
    for (const id of list) {
      const meta = inc.files.get(id);
      if (!meta) throw new RtcError("invalid", `unknown file id ${JSON.stringify(id)}`);
      if (accepted.has(id)) throw new RtcError("invalid", `duplicate file id ${JSON.stringify(id)}`);
      const offset = offsets && Object.hasOwn(offsets, id) ? offsets[id]! : 0;
      if (!Number.isSafeInteger(offset) || offset < 0 || offset > meta.size) {
        throw new RtcError("invalid", `invalid offset for ${JSON.stringify(id)}`);
      }
      accepted.set(id, offset);
      if (offset > 0) {
        wire[id] = offset;
        resumes = true;
      }
    }
    if (accepted.size > 0 && !this.opts.sink) throw new RtcError("no-sink", "no file sink configured");
    if (resumes && !this.opts.sink?.readPrefix) {
      throw new RtcError("no-resume", "the sink cannot read back prefixes, so it cannot resume");
    }
    const frames = encodeAnswer({ t: "answer", transferId: inc.transferId, accept: [...accepted.keys()], offsets: wire });
    this.clearDecisionTimer(inc);
    inc.accepted = accepted;
    inc.answered = true;
    for (const frame of frames) this.sendRaw(frame);
  }

  private declineOffer(inc: Incoming): void {
    this.pending(inc);
    this.clearDecisionTimer(inc);
    inc.answered = true;
    this.inc = null;
    this.sendControl({ t: "answer", transferId: inc.transferId, accept: [], offsets: {}, declined: true });
  }

  private async onFileStart(msg: FileMsg): Promise<void> {
    const inc = this.inc;
    if (!inc) {
      if (this.discard !== null) return;
      throw protocolError("file without an accepted transfer");
    }
    if (!inc.answered || inc.current) throw protocolError("unexpected file message");
    const expected = inc.accepted.get(msg.id);
    if (expected === undefined || inc.finished.has(msg.id)) throw protocolError(`file ${JSON.stringify(msg.id)} was not accepted`);
    if (msg.offset !== expected) throw protocolError(`file ${JSON.stringify(msg.id)} starts at the wrong offset`);
    const meta = inc.files.get(msg.id)!;
    const cur: IncomingFile = { meta, received: msg.offset, hasher: sha256.create(), writer: null, error: null };
    inc.current = cur;
    const sink = this.opts.sink;
    const context: SinkContext = { transferId: inc.transferId, peerKey: this._peer!.key };
    try {
      if (!sink) throw new Error("no file sink configured");
      if (msg.offset > 0) {
        if (!sink.readPrefix) throw new Error("sink cannot read prefixes");
        let n = 0;
        for await (const piece of sink.readPrefix(meta, msg.offset, context)) {
          if (inc.cancelled) return;
          n += piece.byteLength;
          if (n > msg.offset) break;
          cur.hasher.update(piece);
        }
        if (n !== msg.offset) throw new Error("stored prefix does not match the resume offset");
      }
      const writer = await sink.open(meta, msg.offset, context);
      if (inc.cancelled) {
        await writer.abort(inc.abortReason).catch(noop);
        return;
      }
      cur.writer = writer;
    } catch (err) {
      // The file fails, the transfer goes on: its bytes are consumed and nacked.
      cur.error = errorMessage(err);
    }
  }

  private async onBinary(chunk: Bytes): Promise<void> {
    const inc = this.inc;
    const cur = inc?.current;
    if (!inc || !cur) {
      if (!inc && this.discard !== null) return; // tail of a cancelled transfer
      throw protocolError("binary frame outside a file");
    }
    if (cur.received + chunk.byteLength > cur.meta.size) {
      const writer = this.takeWriter(inc);
      await writer?.abort("overrun").catch(noop);
      throw new RtcError("overrun", `file ${JSON.stringify(cur.meta.id)} exceeds its declared size`);
    }
    cur.hasher.update(chunk);
    cur.received += chunk.byteLength;
    const writer = cur.writer;
    if (writer && !cur.error) {
      try {
        await writer.write(chunk);
      } catch (err) {
        if (inc.cancelled) return;
        cur.error = errorMessage(err);
        cur.writer = null;
        await writer.abort("error").catch(noop);
      }
    }
    if (inc.cancelled) return;
    inc.processed += chunk.byteLength;
    this.progress("receive", inc.transferId, cur.meta.id, cur.received, cur.meta.size);
    if (inc.processed - inc.reported >= PROGRESS_STEP) {
      inc.reported = inc.processed;
      this.trySend({ t: "progress", transferId: inc.transferId, bytes: inc.processed });
    }
  }

  private async onFileEnd(msg: FileEndMsg): Promise<void> {
    const inc = this.inc;
    if (!inc) {
      if (this.discard !== null) return;
      throw protocolError("file-end without a transfer");
    }
    const cur = inc.current;
    if (!cur || cur.meta.id !== msg.id) throw protocolError("file-end for a file that is not open");
    const writer = this.takeWriter(inc);
    inc.current = null;
    inc.finished.add(msg.id);
    const digest = toHex(cur.hasher.digest());
    let error = cur.error;
    if (!error && cur.received !== cur.meta.size) error = "size mismatch";
    if (!error && digest !== msg.sha256) error = "sha256 mismatch";
    if (error) {
      await writer?.abort(cur.error ? "error" : "integrity").catch(noop);
    } else if (writer) {
      try {
        await writer.close();
      } catch (err) {
        error = `commit failed: ${errorMessage(err)}`;
      }
    } else {
      error = "file was not opened";
    }
    if (inc.cancelled) return;
    const ok = error === null;
    const ack: FileAckMsg = { t: "file-ack", id: msg.id, ok, sha256: digest };
    if (error !== null) ack.error = error.slice(0, MAX_REASON_LENGTH);
    this.sendControl(ack);
    (ok ? inc.completed : inc.failed).push(msg.id);
    const ev: FileComplete = { transferId: inc.transferId, direction: "receive", fileId: msg.id, ok, sha256: digest };
    if (error !== null) ev.error = error;
    this.emit("fileComplete", ev);
  }

  private onDone(msg: DoneMsg): void {
    const inc = this.inc;
    if (!inc) {
      if (this.discard !== null) return; // crossed our cancel
      throw protocolError("done without a transfer");
    }
    if (inc.transferId !== msg.transferId) throw protocolError("done for another transfer");
    if (!inc.answered || inc.current) throw protocolError("unexpected done");
    for (const id of inc.accepted.keys()) {
      if (inc.finished.has(id)) continue;
      inc.failed.push(id);
      this.emit("fileComplete", { transferId: inc.transferId, direction: "receive", fileId: id, ok: false, error: "not sent" });
    }
    this.inc = null;
    const skipped = inc.metas.filter((m) => !inc.accepted.has(m.id)).map((m) => m.id);
    this.emit("done", { transferId: inc.transferId, direction: "receive", completed: inc.completed, failed: inc.failed, skipped });
  }

  /** Applies a remote cancel; false when it names no active transfer. */
  private onRemoteCancel(msg: CancelMsg): boolean {
    const out = this.out;
    if (out && out.transferId === msg.transferId) {
      this.abortOutgoing(out, new RtcError("cancelled", msg.reason ?? "cancelled by the peer"), true, msg.reason);
      return true;
    }
    const inc = this.inc;
    if (inc && inc.transferId === msg.transferId) {
      this.abortIncoming(inc, true, msg.reason);
      return true;
    }
    return false;
  }

  private abortIncoming(inc: Incoming, byRemote: boolean, reason?: string): void {
    if (inc.cancelled) return;
    inc.cancelled = true;
    inc.abortReason = "cancelled";
    this.clearDecisionTimer(inc);
    if (this.inc === inc) this.inc = null;
    this.discard = inc.transferId;
    const writer = this.takeWriter(inc);
    void writer?.abort("cancelled").catch(noop);
    if (!byRemote) {
      const cancel: CancelMsg = { t: "cancel", transferId: inc.transferId };
      if (reason !== undefined) cancel.reason = reason.slice(0, MAX_REASON_LENGTH);
      this.trySend(cancel);
    }
    if (inc.announced) this.emit("cancelled", cancelledEvent(inc.transferId, "receive", byRemote, false, reason));
  }

  private clearDecisionTimer(inc: Incoming): void {
    if (inc.timer !== null) clearTimeout(inc.timer);
    inc.timer = null;
  }

  private takeWriter(inc: Incoming): SinkWriter | null {
    const cur = inc.current;
    if (!cur) return null;
    const writer = cur.writer;
    cur.writer = null;
    return writer;
  }
}

function cancelledEvent(
  transferId: string,
  direction: Direction,
  byRemote: boolean,
  interrupted: boolean,
  reason?: string,
): TransferCancelled {
  const ev: TransferCancelled = { transferId, direction, byRemote, interrupted };
  if (reason !== undefined) ev.reason = reason;
  return ev;
}
