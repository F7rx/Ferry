// The desktop store against a mocked Tauri bridge: startup ordering, resync
// after missed events, and the previews the shell grants.
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  DeviceSummary,
  EngineEvent,
  HistoryEntry,
  IncomingRequest,
  PairingRequest,
  Snapshot,
  TransferSummary,
} from "../platform/types";

type Listener = (e: { payload: unknown }) => void;

const tauri = vi.hoisted(() => ({
  listeners: new Map<string, Listener>(),
  unlistened: [] as string[],
  failListen: null as string | null,
  calls: [] as string[],
  commands: {} as Record<string, (args?: Record<string, unknown>) => unknown>,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, cb: Listener) => {
    if (name === tauri.failListen) throw new Error("listen failed");
    tauri.listeners.set(name, cb);
    return () => {
      tauri.unlistened.push(name);
      tauri.listeners.delete(name);
    };
  }),
}));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    tauri.calls.push(cmd);
    const run = tauri.commands[cmd];
    if (!run) throw { code: "unknown", message: `no command ${cmd}` };
    return run(args);
  }),
  convertFileSrc: (path: string) => `http://asset.localhost/${encodeURIComponent(path)}`,
}));
vi.mock("@tauri-apps/api/webview", () => ({ getCurrentWebview: () => ({ onDragDropEvent: async () => () => {} }) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ readText: vi.fn(), writeText: vi.fn() }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn(),
  requestPermission: vi.fn(),
  sendNotification: vi.fn(),
}));
vi.mock("../platform", async () => {
  const { createNativePlatform } = await import("../platform/native");
  const p = createNativePlatform();
  return { platform: p, native: p, web: null, demo: null };
});

type Store = typeof import("./engine");
let engine: Store;
let platform: typeof import("../platform").platform;

const peer = { id: "peer", alias: "Phone", deviceKind: "mobile", deviceModel: null, verified: true } as const;

function device(id: string, online = true): DeviceSummary {
  return { ...peer, id, alias: id, protocol: "https", isFerry: true, trusted: false, favorite: false, mine: false, online, lastSeenMs: 0, address: null, ipVersion: 4, rttMs: null, customAlias: null, download: false };
}

function transfer(id: string, bytesDone: number, state: TransferSummary["state"] = "transferring"): TransferSummary {
  return { id, direction: "receive", dropId: null, peer, state, fileCount: 1, filesDone: 0, totalBytes: 100, bytesDone, speedBps: 0, etaSecs: null, startedAtMs: 1, finishedAtMs: null, connection: null, resumable: false, canPause: false, canResume: false, title: "photo.png", text: null, error: null, saveDir: null };
}

function request(id: string, text: string | null = null): IncomingRequest {
  return { id, peer, files: [], totalBytes: 0, text, receivedAtMs: 0, trusted: false, defaultSaveDir: "", expiresAtMs: Date.now() + 60_000 };
}

function pairing(id: string): PairingRequest {
  return { id, peer, code: "123 456", expiresAtMs: Date.now() + 60_000 };
}

function historyEntry(id: number, path: string): HistoryEntry {
  return { id, transferId: "t", direction: "receive", peerId: "peer", peerAlias: "Phone", peerKind: "mobile", kind: "file", name: "photo.png", size: 3, mime: "image/png", path, text: null, timestampMs: 0, status: "completed", verified: true };
}

function snapshot(over: Partial<Snapshot> = {}): Snapshot {
  return {
    local: { alias: "Desk", fingerprint: "fp", deviceKind: "desktop", deviceModel: null, port: 53317, protocol: "https", addresses: [], shortId: "fp", appVersion: "0" },
    devices: [],
    transfers: [],
    settings: { saveDir: null } as Snapshot["settings"],
    server: { running: true, port: 53317, error: null },
    pendingRequests: [],
    pairingRequests: [],
    rooms: [],
    signaling: { url: null, state: "off", error: null, identityKey: "k" },
    browserLinks: [],
    ...over,
  };
}

function emit(event: EngineEvent) {
  tauri.listeners.get("ferry://event")!({ payload: event });
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

let history: { entries: HistoryEntry[]; previewable: string[] };

beforeEach(async () => {
  vi.resetModules();
  tauri.listeners.clear();
  tauri.unlistened.length = 0;
  tauri.failListen = null;
  tauri.calls.length = 0;
  history = { entries: [], previewable: [] };
  tauri.commands = {
    snapshot: () => snapshot(),
    history: () => history,
    take_pending_paths: () => ["C:\\Users\\me\\a.txt"],
    respond: () => true,
    respond_pairing: () => true,
  };
  engine = await import("./engine");
  platform = (await import("../platform")).platform;
});

describe("startup", () => {
  it("listens before asking for the snapshot and keeps events that beat it", async () => {
    const snap = deferred<Snapshot>();
    tauri.commands.snapshot = () => {
      // Every listener is in place before the snapshot is read.
      expect([...tauri.listeners.keys()].sort()).toEqual(["ferry://event", "ferry://previewable", "ferry://resync"]);
      return snap.promise;
    };
    const ready = engine.initEngine();
    await vi.waitFor(() => expect(tauri.calls).toContain("snapshot"));

    // Newer than what the snapshot will say.
    emit({ type: "transferUpdated", transfer: transfer("t1", 80) });
    emit({ type: "deviceUpdated", device: device("late") });
    emit({ type: "incomingRequest", request: request("r-late") });
    snap.resolve(snapshot({ transfers: [transfer("t1", 10)], devices: [device("d1")] }));

    expect(await ready).toBe(true);
    expect(engine.store.ready).toBe(true);
    expect(engine.store.transfers.get("t1")!.bytesDone).toBe(80);
    expect([...engine.store.devices.keys()].sort()).toEqual(["d1", "late"]);
    expect(engine.store.requests.map((r) => r.id)).toEqual(["r-late"]);
  });

  it("never hands launch paths to the snapshot or a resync", async () => {
    await engine.initEngine();
    tauri.listeners.get("ferry://resync")!({ payload: null });
    await vi.waitFor(() => expect(tauri.calls.filter((c) => c === "snapshot")).toHaveLength(2));
    expect(tauri.calls).not.toContain("take_pending_paths");
    const { native } = await import("../platform");
    expect(await native!.takePendingPaths()).toEqual(["C:\\Users\\me\\a.txt"]);
  });

  it("shows a retry when loading fails, and recovers", async () => {
    tauri.commands.snapshot = () => {
      throw { code: "boom", message: "The engine is not running" };
    };
    expect(await engine.initEngine()).toBe(false);
    expect(engine.store.ready).toBe(false);
    expect(engine.store.syncError).toBe("The engine is not running");
    const failure = engine.store.toasts.find((t) => t.action?.label === "Retry");
    expect(failure).toBeDefined();

    tauri.commands.snapshot = () => snapshot({ devices: [device("d1")] });
    failure!.action!.run();
    await vi.waitFor(() => expect(engine.store.ready).toBe(true));
    expect(engine.store.syncError).toBeNull();
    expect(engine.store.toasts.some((t) => t.action?.label === "Retry")).toBe(false);
    expect(engine.store.devices.has("d1")).toBe(true);
    // Retrying did not subscribe twice.
    expect(tauri.unlistened).toEqual([]);
  });

  it("reports a listener that can't be installed and removes the others", async () => {
    tauri.failListen = "ferry://resync";
    expect(await engine.initEngine()).toBe(false);
    expect(engine.store.syncError).toBe("listen failed");
    expect(tauri.calls).not.toContain("snapshot");
    expect(tauri.unlistened.sort()).toEqual(["ferry://event", "ferry://previewable"]);
  });
});

describe("resync", () => {
  async function lag(next: Snapshot) {
    const before = tauri.calls.filter((c) => c === "snapshot").length;
    tauri.commands.snapshot = () => next;
    tauri.listeners.get("ferry://resync")!({ payload: null });
    await vi.waitFor(() => expect(tauri.calls.filter((c) => c === "snapshot").length).toBe(before + 1));
    await vi.waitFor(() => expect(tauri.calls.at(-1)).toBe("history"));
    await new Promise((r) => setTimeout(r, 0));
  }

  it("replaces devices and transfers, dropping ones that are gone", async () => {
    tauri.commands.snapshot = () => snapshot({ devices: [device("d1"), device("d2")], transfers: [transfer("t1", 10), transfer("t2", 10)] });
    await engine.initEngine();
    engine.store.files.set("t2", []);

    await lag(snapshot({ devices: [device("d2", false)], transfers: [transfer("t1", 100, "completed")] }));
    expect([...engine.store.devices.keys()]).toEqual(["d2"]);
    expect(engine.store.devices.get("d2")!.online).toBe(false);
    expect([...engine.store.transfers.keys()]).toEqual(["t1"]);
    expect(engine.store.transfers.get("t1")!.state).toBe("completed");
    expect(engine.store.files.has("t2")).toBe(false);
  });

  it("restores prompts whose events were lost", async () => {
    await engine.initEngine();
    await lag(snapshot({ pendingRequests: [request("r1")], pairingRequests: [pairing("p1")], server: { running: false, port: 53318, error: "in use" } }));
    expect(engine.store.requests.map((r) => r.id)).toEqual(["r1"]);
    expect(engine.store.pairing.requests.map((r) => r.id)).toEqual(["p1"]);
    expect(engine.store.server).toEqual({ running: false, port: 53318, error: "in use" });
  });

  it("drops prompts that expired or closed while events were lost", async () => {
    await engine.initEngine();
    emit({ type: "incomingRequest", request: request("r1") });
    emit({ type: "pairingRequest", request: pairing("p1") });
    expect(engine.store.requests).toHaveLength(1);
    await lag(snapshot());
    expect(engine.store.requests).toEqual([]);
    expect(engine.store.pairing.requests).toEqual([]);
  });

  it("doesn't bring back a prompt answered here that the engine hasn't closed yet", async () => {
    await engine.initEngine();
    emit({ type: "incomingRequest", request: request("r1") });
    emit({ type: "pairingRequest", request: pairing("p1") });
    await engine.respond(engine.store.requests[0]!, { decline: true });
    const { answerPairing } = await import("./pairing");
    await answerPairing(engine.store.pairing.requests[0]!, true);
    await lag(snapshot({ pendingRequests: [request("r1")], pairingRequests: [pairing("p1")] }));
    expect(engine.store.requests).toEqual([]);
    expect(engine.store.pairing.requests).toEqual([]);
  });

  it("restores rooms, links and signaling", async () => {
    await engine.initEngine();
    engine.store.rooms.set("old", { id: "old", link: "x", peers: 0, createdAtMs: 0 });
    await lag(snapshot({ rooms: [{ id: "r", link: "y", peers: 2, createdAtMs: 0 }], signaling: { url: "wss://s", state: "open", error: null, identityKey: "k" } }));
    expect([...engine.store.rooms.keys()]).toEqual(["r"]);
    expect(engine.store.signaling?.state).toBe("open");
  });
});

describe("events", () => {
  it("applies duplicates once", async () => {
    await engine.initEngine();
    const twice = (e: EngineEvent) => {
      emit(e);
      emit(e);
    };
    twice({ type: "incomingRequest", request: request("r1") });
    twice({ type: "incomingRequest", request: request("m1", "hello") });
    twice({ type: "pairingRequest", request: pairing("p1") });
    twice({ type: "historyAdded", entry: historyEntry(7, "C:\\Received\\a.png") });
    twice({ type: "transferUpdated", transfer: transfer("t1", 5) });
    expect(engine.store.requests).toHaveLength(1);
    expect(engine.store.messages).toHaveLength(1);
    expect(engine.store.pairing.requests).toHaveLength(1);
    expect(engine.store.history.filter((h) => h.id === 7)).toHaveLength(1);
    expect(engine.store.transfers.size).toBe(1);
  });

  it("stops listening on teardown", async () => {
    await engine.initEngine();
    engine.stopEngine();
    expect(tauri.unlistened.sort()).toEqual(["ferry://event", "ferry://previewable", "ferry://resync"]);
    expect(tauri.listeners.size).toBe(0);
  });
});

describe("previews", () => {
  const granted = "D:\\Chosen\\photo.png";
  const other = "D:\\Chosen\\other.png";

  it("only for files the shell granted", async () => {
    expect(platform.previewUrl(granted)).toBeNull();
    history = { entries: [historyEntry(1, granted), historyEntry(2, other)], previewable: [granted] };
    await engine.initEngine();
    expect(engine.store.history).toHaveLength(2);
    expect(platform.previewUrl(granted)).toBe(`http://asset.localhost/${encodeURIComponent(granted)}`);
    expect(platform.previewUrl(other)).toBeNull();
  });

  it("for a file granted with a live history entry", async () => {
    const live = "D:\\Live\\new.png";
    await engine.initEngine();
    expect(platform.previewUrl(live)).toBeNull();
    tauri.listeners.get("ferry://previewable")!({ payload: [live] });
    emit({ type: "historyAdded", entry: historyEntry(3, live) });
    expect(platform.previewUrl(live)).not.toBeNull();
    expect(platform.previewUrl("D:\\Live\\neighbour.png")).toBeNull();
  });
});

describe("launch paths", () => {
  /** The shell's queue: launches add to it, `take_pending_paths` empties it. */
  let queue: string[];
  const items = (paths: string[]) => paths.map((path) => ({ kind: "path", path, name: path, size: 1, isDir: false }));
  const announce = (paths: string[]) => {
    queue.push(...paths);
    tauri.listeners.get("ferry://paths")?.({ payload: paths });
  };

  // A platform of its own: the handoff happens once per app start.
  const fresh = async () => (await import("../platform/native")).createNativePlatform();

  beforeEach(() => {
    queue = ["launch.txt"];
    tauri.commands.take_pending_paths = () => queue.splice(0);
    tauri.commands.path_info = (args) => (args!.paths as string[]).map((path) => ({ path, name: path, size: 1, isDir: false }));
  });

  it("keeps a second launch that arrives before the window listens", async () => {
    const native = await fresh();
    // Startup handoff first, then a second launch while nothing listens yet.
    expect(await native.takePendingPaths()).toEqual(["launch.txt"]);
    announce(["early.txt"]);
    expect(tauri.listeners.has("ferry://paths")).toBe(false);

    const handler = vi.fn();
    const stop = native.onPaths(handler);
    await vi.waitFor(() => expect(handler).toHaveBeenCalledTimes(1));
    expect(handler).toHaveBeenCalledWith(items(["early.txt"]));

    // Later launches arrive through the event, each once.
    announce(["later.txt"]);
    await vi.waitFor(() => expect(handler).toHaveBeenCalledTimes(2));
    expect(handler).toHaveBeenLastCalledWith(items(["later.txt"]));
    tauri.listeners.get("ferry://paths")!({ payload: ["later.txt"] }); // a repeated announcement
    await new Promise((r) => setTimeout(r, 20));
    expect(handler).toHaveBeenCalledTimes(2);
    stop();
    expect(tauri.unlistened).toContain("ferry://paths");
  });

  it("leaves what launched the app to the startup handoff", async () => {
    const native = await fresh();
    const handler = vi.fn();
    native.onPaths(handler);
    await vi.waitFor(() => expect(tauri.listeners.has("ferry://paths")).toBe(true));
    announce(["second.txt"]);
    await new Promise((r) => setTimeout(r, 20));
    expect(handler).not.toHaveBeenCalled();
    expect(tauri.calls).not.toContain("take_pending_paths");

    // The handoff takes everything queued so far; nothing is delivered twice.
    expect(await native.takePendingPaths()).toEqual(["launch.txt", "second.txt"]);
    await new Promise((r) => setTimeout(r, 20));
    expect(handler).not.toHaveBeenCalled();
    announce(["third.txt"]);
    await vi.waitFor(() => expect(handler).toHaveBeenCalledWith(items(["third.txt"])));
    expect(handler).toHaveBeenCalledTimes(1);
  });
});
