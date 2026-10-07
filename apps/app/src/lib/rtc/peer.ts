// Glue between SignalingClient and RTCPeerConnection: creates the `ferry/1`
// data channel (offerer), answers incoming offers (answerer), trickles ICE in
// both directions (remote candidates are queued until the remote description
// is set) and hands the channel plus both SDP fingerprints to a PeerSession.
//
// Listen to `session` to attach session listeners before any frame is
// processed; `connect()` / `IncomingConnection.accept()` resolve only once the
// peer is authenticated. Undecided incoming requests are capped and time out,
// and only an ordered, reliable `ferry/1` channel is ever handed to a session.

import { randomId } from "./bytes";
import { Emitter } from "./emitter";
import type { Identity } from "./identity";
import { DC_LABEL, parseMaxMessageSize, RtcError, type DeviceInfo } from "./protocol";
import { PeerSession, type FileSink, type PeerSessionOptions } from "./session";
import type { ClientInfo, SignalingClient } from "./signaling";
import { extractFingerprint, type Role } from "./transcript";

export interface PeerConnectorOptions {
  signaling: SignalingClient;
  identity: Identity;
  device: DeviceInfo;
  /**
   * STUN/TURN servers, or a provider called for every new connection, e.g.
   * `async () => (await signaling.fetchTurn())?.iceServers ?? []` for
   * ferry-signal's short-lived TURN credentials.
   */
  iceServers?: RTCIceServer[] | (() => RTCIceServer[] | Promise<RTCIceServer[]>);
  sink?: FileSink;
  caps?: string[];
  /** Offer → authenticated session deadline (default 30 s). */
  connectTimeoutMs?: number;
  /** Incoming connections not accepted within this time are cancelled (default 60 s). */
  decisionTimeoutMs?: number;
  /** Session tuning (ping/timeouts, the offer decision timeout, progress throttling). */
  session?: Pick<
    PeerSessionOptions,
    "pingIntervalMs" | "timeoutMs" | "handshakeTimeoutMs" | "decisionTimeoutMs" | "progressIntervalMs"
  >;
  RTCPeerConnection?: typeof RTCPeerConnection;
}

export interface ConnectOptions {
  /** Secret of a link/QR room; both sides must prove it. */
  roomSecret?: Uint8Array;
  /** Expected identity key of the peer (base64url), e.g. a known trusted device. */
  expectedPeerKey?: string;
}

export interface IncomingConnection {
  readonly peer: ClientInfo;
  readonly sessionId: string;
  /** Answers the offer; resolves with the authenticated session. */
  accept(options?: ConnectOptions): Promise<PeerSession>;
  /** Declines (tells the peer via signaling `CANCEL`). Also happens on decision timeout. */
  reject(): void;
}

export interface PeerConnectorEvents {
  /** A peer wants to connect; call `accept()` or `reject()`. Without listeners offers are rejected. */
  incoming: IncomingConnection;
  /**
   * A pending `incoming` request ended without `accept()`/`reject()`: the peer
   * cancelled it, the decision timed out ("timeout") or the connector was disposed.
   */
  withdrawn: { peerId: string; sessionId: string; reason: string };
  /** A session was created (fired before its handshake runs). `connection` carries it (for `getStats`). */
  session: { session: PeerSession; peerId: string; role: Role; connection: RTCPeerConnection | null };
}

interface Entry {
  peerId: string;
  sessionId: string;
  pc: RTCPeerConnection | null;
  remoteSet: boolean;
  ice: (RTCIceCandidateInit | null)[];
  answer: ((sdp: string) => void) | null;
  session: PeerSession | null;
  failed: Promise<never>;
  /** Ends the attempt; tells the peer via signaling `CANCEL` unless `notifyPeer` is false. */
  fail(err: RtcError, notifyPeer?: boolean): void;
  timer: ReturnType<typeof setTimeout> | null;
  done: boolean;
}

const MAX_QUEUED_ICE = 64;
/** Incoming requests waiting for `accept()`/`reject()`; more are rejected right away. */
const MAX_PENDING_INCOMING = 16;
const noop = () => {};

function toRtcError(err: unknown): RtcError {
  if (err instanceof RtcError) return err;
  return new RtcError("webrtc", err instanceof Error ? err.message : String(err));
}

export class PeerConnector extends Emitter<PeerConnectorEvents> {
  private readonly opts: PeerConnectorOptions;
  private readonly entries = new Map<string, Entry>();
  private readonly unsubscribe: (() => void)[];
  private pendingIncoming = 0;

  constructor(options: PeerConnectorOptions) {
    super();
    this.opts = options;
    const s = options.signaling;
    this.unsubscribe = [
      s.on("offer", (m) => this.onOffer(m.peer, m.sessionId, m.sdp)),
      s.on("answer", (m) => this.entries.get(key(m.peer.id, m.sessionId))?.answer?.(m.sdp)),
      s.on("ice", (m) => this.onIce(m.peer.id, m.sessionId, m.candidate)),
      s.on("cancel", (m) => this.entries.get(key(m.peer.id, m.sessionId))?.fail(new RtcError("cancelled", "the peer cancelled"), false)),
    ];
  }

  /** Connects to a signaling peer as the offerer; resolves with an authenticated session. */
  async connect(target: string, sessionId: string = randomId(), options: ConnectOptions = {}): Promise<PeerSession> {
    if (this.entries.has(key(target, sessionId))) throw new RtcError("invalid", "session id already in use");
    const entry = this.track(target, sessionId, null);
    entry.timer = setTimeout(() => entry.fail(new RtcError("timeout", "connection timed out")), this.opts.connectTimeoutMs ?? 30_000);
    try {
      const pc = await this.createPeerConnection(entry);
      const channel = pc.createDataChannel(DC_LABEL, { ordered: true });
      this.trickle(entry, pc);
      const answered = new Promise<string>((resolve) => {
        entry.answer = resolve;
      });
      await race(entry, pc.setLocalDescription(await pc.createOffer()));
      const localSdp = pc.localDescription?.sdp;
      if (!localSdp) throw new RtcError("webrtc", "no local description");
      await race(entry, this.opts.signaling.sendOffer(target, sessionId, localSdp));
      const remoteSdp = await race(entry, answered);
      entry.answer = null;
      await race(entry, pc.setRemoteDescription({ type: "answer", sdp: remoteSdp }));
      this.flushIce(entry);
      const session = this.startSession(entry, channel, "offerer", localSdp, remoteSdp, options);
      await race(entry, session.ready);
      return session;
    } catch (err) {
      throw this.abandon(entry, err);
    } finally {
      if (entry.timer !== null) clearTimeout(entry.timer);
      entry.timer = null;
    }
  }

  /** Stops listening to signaling and closes every pending or open session. */
  dispose(): void {
    for (const off of this.unsubscribe) off();
    for (const entry of [...this.entries.values()]) entry.fail(new RtcError("closed", "connector disposed"));
  }

  private onOffer(peer: ClientInfo, sessionId: string, sdp: string): void {
    if (this.entries.has(key(peer.id, sessionId))) return; // duplicate
    if (this.pendingIncoming >= MAX_PENDING_INCOMING) {
      this.opts.signaling.sendCancel(peer.id, sessionId).catch(noop);
      return;
    }
    const entry = this.track(peer.id, sessionId, null);
    this.pendingIncoming++;
    let decided = false;
    const decide = () => {
      decided = true;
      this.pendingIncoming--;
      if (entry.timer !== null) clearTimeout(entry.timer);
      entry.timer = null;
    };
    const withdraw = (reason: string) => {
      if (decided) return;
      decide();
      this.emit("withdrawn", { peerId: peer.id, sessionId, reason });
    };
    // A peer CANCEL (or dispose) before the decision also ends the pending state.
    entry.failed.catch((err: unknown) => withdraw(err instanceof Error ? err.message : String(err)));
    const request: IncomingConnection = {
      peer,
      sessionId,
      accept: (options = {}) => {
        if (decided || entry.done) return Promise.reject(new RtcError("invalid-state", "connection request is no longer pending"));
        decide();
        return this.answer(entry, sdp, options);
      },
      reject: () => {
        if (decided || entry.done) return;
        decide();
        entry.fail(new RtcError("rejected", "connection rejected"));
      },
    };
    entry.timer = setTimeout(() => {
      if (decided || entry.done) return;
      request.reject();
      this.emit("withdrawn", { peerId: peer.id, sessionId, reason: "timeout" });
    }, this.opts.decisionTimeoutMs ?? 60_000);
    if (this.listenerCount("incoming") === 0) {
      request.reject();
      return;
    }
    this.emit("incoming", request);
  }

  private async answer(entry: Entry, remoteSdp: string, options: ConnectOptions): Promise<PeerSession> {
    entry.timer = setTimeout(() => entry.fail(new RtcError("timeout", "connection timed out")), this.opts.connectTimeoutMs ?? 30_000);
    try {
      const pc = await this.createPeerConnection(entry);
      this.trickle(entry, pc);
      let localSdp: string | null = null;
      const opened = new Promise<PeerSession>((resolve, reject) => {
        pc.ondatachannel = (ev) => {
          const channel = ev.channel;
          const reliable = channel.ordered && channel.maxRetransmits === null && channel.maxPacketLifeTime === null;
          if (channel.label !== DC_LABEL || !reliable || entry.session || localSdp === null) {
            channel.close();
            return;
          }
          try {
            // Synchronously, so no frame can arrive before the session listens.
            resolve(this.startSession(entry, channel, "answerer", localSdp, remoteSdp, options));
          } catch (err) {
            reject(err);
          }
        };
      });
      opened.catch(noop);
      await race(entry, pc.setRemoteDescription({ type: "offer", sdp: remoteSdp }));
      this.flushIce(entry);
      await race(entry, pc.setLocalDescription(await pc.createAnswer()));
      localSdp = pc.localDescription?.sdp ?? null;
      if (!localSdp) throw new RtcError("webrtc", "no local description");
      await race(entry, this.opts.signaling.sendAnswer(entry.peerId, entry.sessionId, localSdp));
      const session = await race(entry, opened);
      await race(entry, session.ready);
      return session;
    } catch (err) {
      throw this.abandon(entry, err);
    } finally {
      if (entry.timer !== null) clearTimeout(entry.timer);
      entry.timer = null;
    }
  }

  private startSession(
    entry: Entry,
    channel: RTCDataChannel,
    role: Role,
    localSdp: string,
    remoteSdp: string,
    options: ConnectOptions,
  ): PeerSession {
    const localFingerprint = extractFingerprint(localSdp);
    const remoteFingerprint = extractFingerprint(remoteSdp);
    if (!localFingerprint || !remoteFingerprint) throw new RtcError("webrtc", "SDP without a DTLS fingerprint");
    const session = new PeerSession({
      ...this.opts.session,
      channel,
      role,
      sessionId: entry.sessionId,
      localFingerprint,
      remoteFingerprint,
      identity: this.opts.identity,
      device: this.opts.device,
      caps: this.opts.caps,
      sink: this.opts.sink,
      maxMessageSize: parseMaxMessageSize(remoteSdp),
      roomSecret: options.roomSecret,
      expectedPeerKey: options.expectedPeerKey,
    });
    entry.session = session;
    session.on("closed", () => this.cleanup(entry));
    this.emit("session", { session, peerId: entry.peerId, role, connection: entry.pc });
    return session;
  }

  /** Creates the attempt's RTCPeerConnection (after resolving an `iceServers` provider). */
  private async createPeerConnection(entry: Entry): Promise<RTCPeerConnection> {
    const servers = this.opts.iceServers;
    const iceServers = typeof servers === "function" ? await race(entry, Promise.resolve().then(servers)) : (servers ?? []);
    if (entry.done) throw new RtcError("closed", "connection attempt ended");
    const PC = this.opts.RTCPeerConnection ?? globalThis.RTCPeerConnection;
    const pc = new PC({ iceServers });
    entry.pc = pc;
    return pc;
  }

  private track(peerId: string, sessionId: string, pc: RTCPeerConnection | null): Entry {
    let reject!: (err: RtcError) => void;
    const failed = new Promise<never>((_, rej) => {
      reject = rej;
    });
    failed.catch(noop);
    const entry: Entry = {
      peerId,
      sessionId,
      pc,
      remoteSet: false,
      ice: [],
      answer: null,
      session: null,
      failed,
      fail: (err, notifyPeer = true) => {
        if (entry.done) return;
        // Local failures (timeout, WebRTC failure, reject, dispose) of an attempt
        // must reach the peer, or its side would linger until its own timeout.
        // An established session needs no CANCEL: its data channel closes.
        const established = entry.session?.state === "ready";
        if (notifyPeer && !established) this.opts.signaling.sendCancel(entry.peerId, entry.sessionId).catch(noop);
        reject(err);
        this.cleanup(entry, err.message);
      },
      timer: null,
      done: false,
    };
    this.entries.set(key(peerId, sessionId), entry);
    return entry;
  }

  private abandon(entry: Entry, err: unknown): RtcError {
    const e = toRtcError(err);
    entry.fail(e); // no-op when the attempt already ended
    return e;
  }

  private cleanup(entry: Entry, message?: string): void {
    entry.done = true;
    if (entry.timer !== null) clearTimeout(entry.timer);
    entry.timer = null;
    const k = key(entry.peerId, entry.sessionId);
    if (this.entries.get(k) === entry) this.entries.delete(k);
    if (entry.session && entry.session.state !== "closed") entry.session.close(message);
    try {
      entry.pc?.close();
    } catch {
      // ignore
    }
  }

  private trickle(entry: Entry, pc: RTCPeerConnection): void {
    pc.onicecandidate = (ev) => {
      if (entry.done) return;
      this.opts.signaling.sendIce(entry.peerId, entry.sessionId, ev.candidate ? ev.candidate.toJSON() : null).catch(noop);
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed") entry.fail(new RtcError("webrtc", "WebRTC connection failed"));
    };
  }

  private onIce(peerId: string, sessionId: string, candidate: RTCIceCandidateInit | null): void {
    const entry = this.entries.get(key(peerId, sessionId));
    if (!entry || entry.done) return;
    if (!entry.pc || !entry.remoteSet) {
      if (entry.ice.length < MAX_QUEUED_ICE) entry.ice.push(candidate);
      return;
    }
    addIce(entry.pc, candidate);
  }

  private flushIce(entry: Entry): void {
    entry.remoteSet = true;
    const queued = entry.ice;
    entry.ice = [];
    if (entry.pc) for (const c of queued) addIce(entry.pc, c);
  }
}

function key(peerId: string, sessionId: string): string {
  return `${peerId}\n${sessionId}`;
}

function race<T>(entry: Entry, p: Promise<T>): Promise<T> {
  return Promise.race([p, entry.failed]);
}

function addIce(pc: RTCPeerConnection, candidate: RTCIceCandidateInit | null): void {
  (candidate ? pc.addIceCandidate(candidate) : pc.addIceCandidate()).catch(noop);
}
