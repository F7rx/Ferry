// Test helpers for the browser platform (imported by *.test.ts only): a fresh
// IndexedDB per simulated browser, the few DOM globals web.ts touches, and
// remote Ferry peers built from the real rtc library over the fake signaling
// server and RTCPeerConnection.
import "fake-indexeddb/auto";
import { IDBFactory } from "fake-indexeddb";
import { vi } from "vitest";
import { exportPublicKey, generateIdentity, type Identity } from "../lib/rtc/identity";
import { PeerConnector } from "../lib/rtc/peer";
import type { IncomingOffer, PeerSession } from "../lib/rtc/session";
import { SignalingClient, type ClientInfo } from "../lib/rtc/signaling";
import { FakePeerConnection, FakeSignalServer, MemorySink, nextEvent, waitUntil } from "../lib/rtc/test-fakes";
import type { EngineEvent, Settings } from "./types";

/** Minimal window/location/document for code paths web.ts reaches in tests. */
export function installBrowserGlobals() {
  const g = globalThis as Record<string, unknown>;
  g.location ??= { protocol: "https:", host: "ferry.test", origin: "https://ferry.test", hash: "", pathname: "/", search: "" };
  g.window ??= { addEventListener() {}, removeEventListener() {}, history: { replaceState() {} }, isSecureContext: true, open() {} };
  g.document ??= { createElement: () => ({ click() {}, remove() {}, style: {} }), body: { append() {} } };
  g.confirm ??= () => true;
}

/** A browser profile: its own IndexedDB. Use with `loadWeb` to open the same profile again (a reload). */
export function newProfile(): IDBFactory {
  return new IDBFactory();
}

/**
 * Loads a fresh copy of web.ts (and its storage modules) bound to `profile`,
 * like a page load. Each copy keeps its own database connection.
 */
export async function loadWeb(profile: IDBFactory) {
  installBrowserGlobals();
  vi.resetModules();
  (globalThis as { indexedDB: IDBFactory }).indexedDB = profile;
  const web = await import("./web");
  const sink = await import("./web-sink");
  const idb = await import("../lib/idb");
  return { web, sink, idb };
}

export interface Browser {
  platform: ReturnType<(typeof import("./web"))["createWebPlatform"]>;
  sink: typeof import("./web-sink");
  idb: typeof import("../lib/idb");
  key: string;
  events: EngineEvent[];
  profile: IDBFactory;
  settings: Settings;
}

/** Changes some settings of `b` (the platform takes whole settings). */
export async function setSettings(b: Browser, patch: Partial<Settings>) {
  b.settings = await b.platform.updateSettings({ ...b.settings, ...patch });
}

/** Starts a browser platform on `server`, waiting until it is connected. */
export async function startBrowser(
  server: FakeSignalServer,
  profile = newProfile(),
  before?: (b: Omit<Browser, "key" | "settings">) => Promise<void> | void,
  options: { retryDelaysMs?: readonly number[] } = {},
): Promise<Browser> {
  const { web, sink, idb } = await loadWeb(profile);
  const platform = web.createWebPlatform({
    WebSocket: server.WebSocket,
    RTCPeerConnection: FakePeerConnection as unknown as typeof RTCPeerConnection,
    ...options,
  });
  const events: EngineEvent[] = [];
  platform.subscribe((e) => events.push(e));
  await before?.({ platform, sink, idb, events, profile });
  const snap = await platform.init();
  await waitUntil(() => events.some((e) => e.type === "signalingStatus" && e.status.state === "open"), 5000, "signaling open");
  return { platform, sink, idb, key: snap.local.fingerprint, events, profile, settings: snap.settings };
}

export interface Remote {
  key: string;
  identity: Identity;
  signaling: SignalingClient;
  connector: PeerConnector;
  sink: MemorySink;
  /** Offers this peer received, in order. */
  offers: IncomingOffer[];
  /** Signaling id of a browser (by identity key) as this peer sees it. */
  idOf(key: string): Promise<string>;
  connect(key: string): Promise<PeerSession>;
  close(): void;
}

/** A Ferry peer using the rtc library directly (like another browser or the native app). */
export async function startRemote(server: FakeSignalServer, alias: string, onOffer?: (offer: IncomingOffer, session: PeerSession) => void): Promise<Remote> {
  const identity = await generateIdentity("ed25519");
  const key = await exportPublicKey(identity);
  const seen = new Map<string, ClientInfo>();
  const signaling = new SignalingClient({
    url: "wss://signal.test/v1/ws",
    info: { alias, deviceType: "desktop", token: alias, publicKey: key },
    WebSocket: server.WebSocket,
  });
  const note = (p: ClientInfo) => p.ext?.key && seen.set(p.ext.key, p);
  signaling.on("hello", ({ peers }) => peers.forEach(note));
  signaling.on("join", ({ peer }) => note(peer));
  const hello = nextEvent(signaling, "hello");
  signaling.connect();
  await hello;
  const sink = new MemorySink();
  const connector = new PeerConnector({
    signaling,
    identity,
    device: { alias, deviceType: "desktop", platform: "test" },
    sink,
    RTCPeerConnection: FakePeerConnection as unknown as typeof RTCPeerConnection,
  });
  const offers: IncomingOffer[] = [];
  connector.on("incoming", (req) => void req.accept().catch(() => {}));
  connector.on("session", ({ session }) =>
    session.on("offer", (offer) => {
      offers.push(offer);
      if (onOffer) onOffer(offer, session);
      else offer.accept();
    }),
  );
  return {
    key,
    identity,
    signaling,
    connector,
    sink,
    offers,
    async idOf(k) {
      await waitUntil(() => seen.has(k), 5000, `${alias} sees the browser`);
      return seen.get(k)!.id;
    },
    async connect(this: Remote, k: string) {
      return connector.connect(await this.idOf(k), undefined, { expectedPeerKey: k });
    },
    close() {
      connector.dispose();
      signaling.close();
    },
  };
}
