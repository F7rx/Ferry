// Demo platform: simulated devices and transfers for development and visual
// QA. Never used in production builds unless `?demo` is in the URL, and the
// UI labels it as a preview.
import type {
  Capabilities,
  DeviceSummary,
  DiagnosticCheck,
  EngineEvent,
  HistoryEntry,
  IncomingRequest,
  LocalDevice,
  OutgoingItem,
  Platform,
  Settings,
  TransferFile,
  TransferSummary,
  BrowserLink,
} from "./types";

const now = () => Date.now();
const uid = () => Math.random().toString(36).slice(2, 10);

const local: LocalDevice = {
  alias: "Studio PC",
  fingerprint: "8F3A2C91D4E7B6A50C1F9E8D7B6A5C4F3E2D1C0B9A8F7E6D5C4B3A2F1E0D9C8B",
  deviceKind: "desktop",
  deviceModel: "Windows",
  port: 53317,
  protocol: "https",
  addresses: ["192.168.1.24"],
  shortId: "8F3A 2C91 D4E7 B6A5",
  appVersion: "0.1.0",
};

function dev(p: Partial<DeviceSummary> & Pick<DeviceSummary, "id" | "alias" | "deviceKind">): DeviceSummary {
  return {
    deviceModel: null,
    verified: true,
    protocol: "https",
    isFerry: true,
    trusted: false,
    favorite: false,
    mine: false,
    online: true,
    lastSeenMs: now(),
    address: "192.168.1.31:53317",
    ipVersion: 4,
    rttMs: 4,
    customAlias: null,
    download: false,
    ...p,
  };
}

const devices: DeviceSummary[] = [
  dev({ id: "A1", alias: "Maya’s iPhone", deviceKind: "mobile", deviceModel: "iPhone", mine: true, trusted: true, favorite: true, rttMs: 6 }),
  dev({ id: "B2", alias: "Studio MacBook Pro", deviceKind: "desktop", deviceModel: "macOS", trusted: true, favorite: true, rttMs: 3 }),
  dev({ id: "C3", alias: "Pixel 9", deviceKind: "mobile", deviceModel: "Android", isFerry: false, rttMs: 11 }),
  dev({ id: "D4", alias: "Living Room iPad", deviceKind: "mobile", deviceModel: "iPad", rttMs: 9 }),
  dev({ id: "E5", alias: "Basement NAS", deviceKind: "server", deviceModel: "Linux", isFerry: false, protocol: "http", verified: false, rttMs: 2 }),
];

const settings: Settings = {
  version: 1,
  alias: local.alias,
  deviceKind: null,
  deviceModel: null,
  receiveEnabled: true,
  saveDir: null,
  autoAccept: "myDevices",
  pin: null,
  decisionTimeoutSecs: 300,
  historyEnabled: true,
  keepMessageText: false,
  checksumsForLocalsend: false,
  verifyIncomingChecksums: true,
  parallelFiles: 8,
  port: 53317,
  encryption: true,
  multicastGroup: "224.0.0.167",
  ipv6: true,
  includeVirtualInterfaces: false,
  interfaceWhitelist: null,
  interfaceBlacklist: null,
  subnetScan: true,
  signalingUrl: null,
  stunServers: ["stun:stun.l.google.com:19302"],
};

const history: HistoryEntry[] = [
  ["IMG_2041.HEIC", 3_800_000, "image/heic", "receive", "Maya’s iPhone", "mobile", 4],
  ["Quarterly review.key", 48_200_000, "application/octet-stream", "send", "Studio MacBook Pro", "desktop", 26],
  ["Boarding pass.pdf", 220_000, "application/pdf", "receive", "Pixel 9", "mobile", 75],
  ["Studio session 03.wav", 412_000_000, "audio/wav", "receive", "Studio MacBook Pro", "desktop", 190],
  ["screenshot 2026-10-03.png", 1_400_000, "image/png", "send", "Maya’s iPhone", "mobile", 900],
].map(([name, size, mime, direction, peerAlias, peerKind, minsAgo], i) => ({
  id: 100 - i,
  transferId: uid(),
  direction: direction as "send" | "receive",
  peerId: "A1",
  peerAlias: peerAlias as string,
  peerKind: peerKind as "mobile",
  kind: "file" as const,
  name: name as string,
  size: size as number,
  mime: mime as string,
  path: `C:/Users/demo/Downloads/Ferry/${name}`,
  text: null,
  timestampMs: now() - (minsAgo as number) * 60_000,
  status: "completed" as const,
  verified: true,
}));

export function createDemoPlatform(): Platform & { trigger(scene: string): void } {
  const handlers = new Set<(e: EngineEvent) => void>();
  const emit = (e: EngineEvent) => handlers.forEach((h) => h(e));
  const transfers = new Map<string, TransferSummary>();
  const files = new Map<string, TransferFile[]>();
  const capabilities: Capabilities = {
    kind: "demo",
    lanDiscovery: true,
    receiveInBackground: false,
    pickFolders: true,
    revealInFolder: false,
    clipboardRead: true,
    nativeNotifications: false,
    tray: false,
    localsendInterop: true,
    browserLinks: true,
    pairing: true,
    remoteLinks: true,
  };

  function simulate(t: TransferSummary, speed: number) {
    const tick = 120;
    let phase = 0;
    const timer = setInterval(() => {
      phase += tick;
      if (t.state === "waitingForAcceptance" && phase > 900) t.state = "transferring";
      if (t.state === "transferring") {
        const jitter = 0.85 + Math.random() * 0.3;
        t.speedBps = Math.round(speed * jitter);
        t.bytesDone = Math.min(t.totalBytes, t.bytesDone + (t.speedBps * tick) / 1000);
        t.etaSecs = t.speedBps ? Math.ceil((t.totalBytes - t.bytesDone) / t.speedBps) : null;
        const per = files.get(t.id) ?? [];
        let left = t.bytesDone;
        let done = 0;
        for (const f of per) {
          const b = Math.min(f.size, left);
          f.bytesDone = b;
          left -= b;
          f.state = b >= f.size ? "done" : b > 0 ? "transferring" : "pending";
          if (f.state === "done") done++;
        }
        t.filesDone = done;
        if (t.bytesDone >= t.totalBytes) {
          t.state = "completed";
          t.speedBps = 0;
          t.etaSecs = null;
          t.finishedAtMs = now();
          clearInterval(timer);
        }
      }
      if (t.state === "paused" || t.state === "cancelled") {
        if (t.state === "cancelled") clearInterval(timer);
        t.speedBps = 0;
      }
      emit({ type: "transferUpdated", transfer: { ...t } });
    }, tick);
  }

  function makeTransfer(direction: "send" | "receive", peer: DeviceSummary, items: { name: string; size: number }[], state: TransferSummary["state"]): TransferSummary {
    const id = uid();
    const total = items.reduce((a, b) => a + b.size, 0);
    const t: TransferSummary = {
      id,
      direction,
      dropId: null,
      peer: { id: peer.id, alias: peer.alias, deviceKind: peer.deviceKind, deviceModel: peer.deviceModel, verified: peer.verified },
      state,
      fileCount: items.length,
      filesDone: 0,
      totalBytes: total,
      bytesDone: 0,
      speedBps: 0,
      etaSecs: null,
      startedAtMs: now(),
      finishedAtMs: null,
      connection: { transport: "lan", encrypted: peer.protocol === "https", ipVersion: 4, relayed: false, address: peer.address },
      resumable: peer.isFerry,
      title: items[0]?.name ?? "Message",
      text: null,
      error: null,
      saveDir: null,
    };
    transfers.set(id, t);
    files.set(
      id,
      items.map((i, n) => ({ id: `f${n}`, name: i.name, size: i.size, mime: "application/octet-stream", state: "pending", bytesDone: 0 })),
    );
    emit({ type: "transferUpdated", transfer: { ...t } });
    return t;
  }

  const platform: Platform & { trigger(scene: string): void } = {
    capabilities,
    async init() {
      setTimeout(() => devices.forEach((d, i) => setTimeout(() => emit({ type: "deviceUpdated", device: d }), 180 * i)), 300);
      return { local, devices: [], transfers: [], settings: { ...settings }, server: { running: true, port: 53317, error: null } };
    },
    subscribe(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
    async send(targets, items) {
      const sized = items.map((i) => ({
        name: i.name,
        size: i.kind === "path" ? (i.size ?? 24_000_000) : i.kind === "text" ? i.text.length : i.size,
      }));
      return targets.map((target) => {
        const peer = devices.find((d) => d.id === target.id) ?? devices[0]!;
        const t = makeTransfer("send", peer, sized, "waitingForAcceptance");
        simulate(t, peer.deviceKind === "mobile" ? 38_000_000 : 92_000_000);
        return t.id;
      });
    },
    async respond(requestId, decision) {
      emit({ type: "incomingRequestClosed", id: requestId, reason: "answered" });
      if (decision.decline) return true;
      const peer = devices[1]!;
      const t = makeTransfer(
        "receive",
        peer,
        [
          { name: "Cover art final.png", size: 8_400_000 },
          { name: "Mixdown v7.wav", size: 182_000_000 },
          { name: "Lyrics.txt", size: 4_000 },
        ],
        "transferring",
      );
      simulate(t, 74_000_000);
      return true;
    },
    async cancel(id) {
      const t = transfers.get(id);
      if (t) {
        t.state = "cancelled";
        t.finishedAtMs = now();
        emit({ type: "transferUpdated", transfer: { ...t } });
      }
      return true;
    },
    async pause(id) {
      const t = transfers.get(id);
      if (t) t.state = "paused";
      return true;
    },
    async resume(id) {
      const t = transfers.get(id);
      if (t) t.state = "transferring";
      return true;
    },
    async submitPin() {
      return true;
    },
    async dismiss(id) {
      transfers.delete(id);
      emit({ type: "transferRemoved", id });
      return true;
    },
    async transferFiles(id) {
      return files.get(id) ?? [];
    },
    async refreshDevices() {},
    async addDevice(host, port) {
      const d = dev({ id: uid(), alias: `Device at ${host}`, deviceKind: "desktop", address: `${host}:${port}` });
      devices.push(d);
      emit({ type: "deviceUpdated", device: d });
      return d;
    },
    async setDeviceFlags(id, flags) {
      const d = devices.find((x) => x.id === id);
      if (!d) return null;
      Object.assign(d, {
        trusted: flags.trusted ?? d.trusted,
        favorite: flags.favorite ?? d.favorite,
        mine: flags.mine ?? d.mine,
        customAlias: flags.customAlias === undefined ? d.customAlias : flags.customAlias,
      });
      emit({ type: "deviceUpdated", device: { ...d } });
      return d;
    },
    async forgetDevice(id) {
      emit({ type: "deviceRemoved", id });
    },
    async signalingStatus() {
      return { url: "wss://signal.example.org/v1/ws", state: "open" as const, error: null, identityKey: "demo" };
    },
    async createRoom() {
      const room = { id: `r:${uid()}${uid()}`, link: `https://ferry.example.org/#room=${uid()}${uid()}${uid()}`, peers: 0, createdAtMs: now() };
      emit({ type: "roomUpdated", room });
      setTimeout(() => emit({ type: "roomUpdated", room: { ...room, peers: 1 } }), 4000);
      return room;
    },
    async joinRoom(link) {
      if (!link.includes("room=")) throw { code: "bad_link", message: "That isn't a Ferry link." };
      const room = { id: `r:${uid()}${uid()}`, link, peers: 1, createdAtMs: now() };
      emit({ type: "roomUpdated", room });
      return room;
    },
    async leaveRoom(id) {
      emit({ type: "roomRemoved", id });
      return true;
    },
    async rooms() {
      return [];
    },
    async createPairingOffer() {
      const offer = { id: uid(), uri: `ferry://pair?v=1&fp=${local.fingerprint}&a=192.168.1.24&p=53317&s=${uid()}${uid()}`, expiresAtMs: now() + 300_000 };
      // Someone scans it a few seconds later.
      setTimeout(() => {
        const d = devices.find((x) => x.id === "D4");
        if (!d) return;
        Object.assign(d, { mine: true, trusted: true });
        emit({ type: "pairingOfferClosed", id: offer.id, device: { ...d } });
      }, 9000);
      return offer;
    },
    async cancelPairingOffer(id) {
      emit({ type: "pairingOfferClosed", id, device: null });
      return true;
    },
    async pairWithUri(uri) {
      if (!uri.trim().startsWith("ferry://pair?")) throw { code: "pair_invalid", message: "That isn't a Ferry pairing code." };
      const d = devices.find((x) => x.id === "B2") ?? devices[0]!;
      Object.assign(d, { mine: true, trusted: true });
      emit({ type: "deviceUpdated", device: { ...d } });
      return { ...d };
    },
    async startCodePairing(deviceId) {
      const d = devices.find((x) => x.id === deviceId);
      if (!d) throw { code: "unknown_device", message: "That device is no longer around." };
      const pairing = { id: uid(), peer: { id: d.id, alias: d.customAlias ?? d.alias, deviceKind: d.deviceKind, deviceModel: d.deviceModel, verified: true }, code: "482 913" };
      setTimeout(() => {
        Object.assign(d, { mine: true, trusted: true });
        emit({ type: "pairingFinished", id: pairing.id, outcome: "paired", device: { ...d }, error: null });
      }, 6000);
      return pairing;
    },
    async cancelCodePairing(id) {
      emit({ type: "pairingFinished", id, outcome: "cancelled", device: null, error: null });
      return true;
    },
    async respondPairing(requestId, accept) {
      emit({ type: "pairingRequestClosed", id: requestId });
      const d = devices.find((x) => x.id === "B2");
      if (accept && d) {
        Object.assign(d, { mine: true, trusted: true });
        emit({ type: "deviceUpdated", device: { ...d } });
      }
      return true;
    },
    async unpairDevice(id) {
      const d = devices.find((x) => x.id === id);
      if (!d) return null;
      Object.assign(d, { mine: false, trusted: false });
      emit({ type: "deviceUpdated", device: { ...d } });
      return { ...d };
    },
    async history(limit, _before, direction) {
      return history.filter((h) => !direction || h.direction === direction).slice(0, limit);
    },
    async deleteHistory() {
      return true;
    },
    async clearHistory() {},
    async updateSettings(s) {
      Object.assign(settings, s);
      return { ...settings };
    },
    async shareWithBrowsers(items, pin) {
      const link: BrowserLink = {
        id: uid(),
        kind: "download",
        urls: [`http://192.168.1.24:53319/s/${uid()}${uid()}${uid()}`],
        expiresAtMs: now() + 3600_000,
        pinRequired: !!pin,
        fileCount: items.length,
        totalBytes: items.reduce((n, i) => n + (i.kind === "path" ? (i.size ?? 0) : i.kind === "text" ? 0 : i.size), 0),
        downloads: 0,
        uploads: 0,
        active: 0,
        recentClients: [],
      };
      emit({ type: "browserShareUpdated", share: link });
      setTimeout(() => emit({ type: "browserShareUpdated", share: { ...link, active: 1, recentClients: ["Safari on iPhone"] } }), 2500);
      setTimeout(() => emit({ type: "browserShareUpdated", share: { ...link, downloads: 1, recentClients: ["Safari on iPhone"] } }), 5000);
      return link;
    },
    async receiveFromBrowsers(pin) {
      const link: BrowserLink = {
        id: uid(), kind: "upload", urls: [`http://192.168.1.24:53319/s/${uid()}${uid()}${uid()}`], expiresAtMs: now() + 3600_000,
        pinRequired: !!pin, fileCount: 0, totalBytes: 0, downloads: 0, uploads: 0, active: 0, recentClients: [],
      };
      emit({ type: "browserShareUpdated", share: link });
      return link;
    },
    async stopBrowserLink(id) {
      emit({ type: "browserShareRemoved", id });
      return true;
    },
    async browserLinks() {
      return [];
    },
    async diagnostics(): Promise<DiagnosticCheck[]> {
      return [
        { id: "network", label: "Local network", status: "ok", value: "Connected (Wi-Fi, 192.168.1.24)" },
        { id: "discovery", label: "Discovery", status: "ok", value: "Working" },
        { id: "port", label: "Listening port", status: "ok", value: "53317" },
        { id: "encryption", label: "Encryption", status: "ok", value: "On (HTTPS, mutual TLS)" },
        { id: "ipv4", label: "IPv4", status: "ok", value: "192.168.1.24" },
        { id: "ipv6", label: "IPv6", status: "ok", value: "Link-local only" },
        { id: "devices", label: "Devices nearby", status: "ok", value: "5" },
        { id: "firewall", label: "Firewall", status: "ok", value: "OK (Private network)" },
        { id: "webrtc", label: "Browser & remote transfers", status: "unknown", value: "Not set up", detail: "Needed only to reach browsers and devices outside this network." },
      ];
    },
    async pickFiles() {
      return [
        { kind: "path", path: "C:/demo/Holiday reel.mp4", name: "Holiday reel.mp4", size: 1_240_000_000, isDir: false },
        { kind: "path", path: "C:/demo/IMG_2088.HEIC", name: "IMG_2088.HEIC", size: 4_100_000, isDir: false },
      ];
    },
    async pickFolder() {
      return [{ kind: "path", path: "C:/demo/Album", name: "Album", size: null, isDir: true }];
    },
    async pickSaveFolder() {
      return "C:/Users/demo/Downloads/Ferry";
    },
    async readClipboard(): Promise<OutgoingItem | null> {
      return { kind: "text", text: "https://example.com/shared-board", name: "Clipboard" };
    },
    async copyText(text) {
      await navigator.clipboard?.writeText(text).catch(() => {});
    },
    async open() {},
    async reveal() {},
    previewUrl: () => null,
    async notify() {},
    trigger(scene) {
      if (scene === "incoming") {
        const peer = devices[1]!;
        const request: IncomingRequest = {
          id: uid(),
          peer: { id: peer.id, alias: peer.alias, deviceKind: peer.deviceKind, deviceModel: peer.deviceModel, verified: true },
          files: [
            { id: "1", name: "Cover art final.png", size: 8_400_000, mime: "image/png" },
            { id: "2", name: "Mixdown v7.wav", size: 182_000_000, mime: "audio/wav" },
            { id: "3", name: "Lyrics.txt", size: 4_000, mime: "text/plain" },
          ],
          totalBytes: 190_404_000,
          text: null,
          receivedAtMs: now(),
          trusted: false,
          defaultSaveDir: "C:/Users/demo/Downloads/Ferry",
          expiresAtMs: now() + 300_000,
        };
        emit({ type: "incomingRequest", request });
      }
      if (scene === "message") {
        const peer = devices[0]!;
        emit({
          type: "incomingRequest",
          request: {
            id: uid(),
            peer: { id: peer.id, alias: peer.alias, deviceKind: peer.deviceKind, deviceModel: peer.deviceModel, verified: true },
            files: [],
            totalBytes: 0,
            text: "https://maps.example.com/studio-entrance. The buzzer is broken, call when you’re here",
            receivedAtMs: now(),
            trusted: true,
            defaultSaveDir: "",
            expiresAtMs: now(),
          },
        });
      }
      if (scene === "many") {
        const extra = [
          dev({ id: "F6", alias: "Studio Mac mini", deviceKind: "desktop", deviceModel: "macOS", rttMs: 3 }),
          dev({ id: "G7", alias: "Lena’s Pixel Tablet", deviceKind: "mobile", deviceModel: "Tablet", rttMs: 14 }),
          dev({ id: "H8", alias: "Front Desk PC", deviceKind: "desktop", deviceModel: "Windows", rttMs: 5 }),
          dev({ id: "I9", alias: "Kai’s Galaxy S26", deviceKind: "mobile", deviceModel: "Android", isFerry: false, rttMs: 20 }),
          dev({ id: "J10", alias: "Render Node", deviceKind: "headless", deviceModel: "Linux", rttMs: 1 }),
          dev({ id: "K11", alias: "Meeting Room TV", deviceKind: "desktop", deviceModel: "Linux", rttMs: 7 }),
        ];
        extra.forEach((d, i) => setTimeout(() => {
          devices.push(d);
          emit({ type: "deviceUpdated", device: d });
        }, 120 * i));
      }
      if (scene === "pair-request") {
        const peer = devices.find((x) => x.id === "B2")!;
        emit({
          type: "pairingRequest",
          request: { id: uid(), peer: { id: peer.id, alias: peer.alias, deviceKind: peer.deviceKind, deviceModel: peer.deviceModel, verified: true }, code: "482 913", expiresAtMs: now() + 120_000 },
        });
      }
      if (scene === "room") {
        void platform.createRoom();
      }
      if (scene === "links") {
        void platform.shareWithBrowsers([
          { kind: "path", path: "a", name: "Site photos", size: 412_000_000, isDir: true },
          { kind: "path", path: "b", name: "Floor plan.pdf", size: 2_300_000, isDir: false },
        ], null);
        void platform.receiveFromBrowsers("482913");
      }
      if (scene === "transfers") {
        platform.send([{ kind: "device", id: "B2" }], [
          { kind: "path", path: "x", name: "Holiday reel.mp4", size: 1_240_000_000, isDir: false },
          { kind: "path", path: "y", name: "IMG_2088.HEIC", size: 4_100_000, isDir: false },
        ]);
        platform.send([{ kind: "device", id: "A1" }], [{ kind: "path", path: "z", name: "Wedding album", size: 3_600_000_000, isDir: true }]);
      }
    },
  };
  return platform;
}
