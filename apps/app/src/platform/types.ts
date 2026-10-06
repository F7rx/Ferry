// The contract between the UI and an engine. Mirrors ferry-core's
// `model.rs` / `events.rs` (serde camelCase). Native builds talk to the Rust
// engine over Tauri IPC; the PWA implements the same surface over WebRTC.

export type DeviceKind = "mobile" | "desktop" | "web" | "headless" | "server";
export type Protocol = "http" | "https";
export type Direction = "send" | "receive";

export interface ErrorInfo {
  code: string;
  message: string;
  hint?: string;
}

export interface LocalDevice {
  alias: string;
  fingerprint: string;
  deviceKind: DeviceKind;
  deviceModel: string | null;
  port: number;
  protocol: Protocol;
  addresses: string[];
  shortId: string;
  appVersion: string;
}

export interface DeviceSummary {
  id: string;
  alias: string;
  deviceModel: string | null;
  deviceKind: DeviceKind;
  verified: boolean;
  protocol: Protocol;
  isFerry: boolean;
  trusted: boolean;
  favorite: boolean;
  mine: boolean;
  online: boolean;
  lastSeenMs: number;
  address: string | null;
  ipVersion: number | null;
  rttMs: number | null;
  customAlias: string | null;
  download: boolean;
}

export type TransferState =
  | "preparing"
  | "waitingForAcceptance"
  | "pinRequired"
  | "transferring"
  | "paused"
  | "reconnecting"
  | "verifying"
  | "completed"
  | "completedWithErrors"
  | "declined"
  | "cancelled"
  | "failed";

export type FileState = "pending" | "transferring" | "verifying" | "done" | "failed" | "skipped" | "cancelled";

export interface PeerRef {
  id: string;
  alias: string;
  deviceKind: DeviceKind;
  deviceModel: string | null;
  verified: boolean;
}

export interface ConnectionInfo {
  transport: "lan" | "webrtc" | "browser" | string;
  encrypted: boolean;
  ipVersion: number | null;
  relayed: boolean;
  address: string | null;
}

export interface TransferFile {
  id: string;
  name: string;
  size: number;
  mime: string;
  state: FileState;
  bytesDone: number;
  error?: ErrorInfo;
  path?: string;
}

export interface TransferSummary {
  id: string;
  direction: Direction;
  dropId: string | null;
  peer: PeerRef;
  state: TransferState;
  fileCount: number;
  filesDone: number;
  totalBytes: number;
  bytesDone: number;
  speedBps: number;
  etaSecs: number | null;
  startedAtMs: number;
  finishedAtMs: number | null;
  connection: ConnectionInfo | null;
  resumable: boolean;
  title: string;
  text: string | null;
  error: ErrorInfo | null;
  saveDir: string | null;
}

export interface IncomingFile {
  id: string;
  name: string;
  size: number;
  mime: string;
}

export interface IncomingRequest {
  id: string;
  peer: PeerRef;
  files: IncomingFile[];
  totalBytes: number;
  text: string | null;
  receivedAtMs: number;
  trusted: boolean;
  defaultSaveDir: string;
  expiresAtMs: number;
}

export interface Decision {
  accept: string[] | null;
  decline: boolean;
  trust: boolean;
  saveDir: string | null;
}

export type HistoryKind = "file" | "text";
export type HistoryStatus = "completed" | "failed" | "cancelled";

export interface HistoryEntry {
  id: number;
  transferId: string;
  direction: Direction;
  peerId: string;
  peerAlias: string;
  peerKind: DeviceKind;
  kind: HistoryKind;
  name: string;
  size: number;
  mime: string;
  path: string | null;
  text: string | null;
  timestampMs: number;
  status: HistoryStatus;
  verified: boolean;
}

export type AutoAccept = "off" | "myDevices" | "trusted";

export interface Settings {
  version: number;
  alias: string;
  deviceKind: DeviceKind | null;
  deviceModel: string | null;
  receiveEnabled: boolean;
  saveDir: string | null;
  autoAccept: AutoAccept;
  pin: string | null;
  decisionTimeoutSecs: number;
  historyEnabled: boolean;
  keepMessageText: boolean;
  checksumsForLocalsend: boolean;
  verifyIncomingChecksums: boolean;
  parallelFiles: number;
  port: number;
  encryption: boolean;
  multicastGroup: string;
  ipv6: boolean;
  includeVirtualInterfaces: boolean;
  interfaceWhitelist: string[] | null;
  interfaceBlacklist: string[] | null;
  subnetScan: boolean;
  signalingUrl: string | null;
  stunServers: string[];
}

export type NoticeLevel = "info" | "warning" | "error";

/** A link any browser on the local network can open (download or upload). */
export interface BrowserLink {
  id: string;
  kind: "download" | "upload";
  urls: string[];
  expiresAtMs: number;
  pinRequired: boolean;
  fileCount: number;
  totalBytes: number;
  downloads: number;
  /** Files received through an upload link. */
  uploads: number;
  active: number;
  recentClients: string[];
}

/** A QR code / link this device shows so another device can pair with it. */
export interface PairingOffer {
  id: string;
  uri: string;
  expiresAtMs: number;
}

/** Another device asks to pair; both screens show `code`. */
export interface PairingRequest {
  id: string;
  peer: PeerRef;
  code: string;
  expiresAtMs: number;
}

/** A pairing request this device sent (code comparison). */
export interface OutgoingPairing {
  id: string;
  peer: PeerRef;
  code: string;
}

export type PairingOutcome = "paired" | "declined" | "cancelled" | "failed";

/** A private link (browser app): whoever opens it can see this device, on any network. */
export interface RoomInfo {
  id: string;
  link: string;
  /** Other devices in the room right now. */
  peers: number;
  createdAtMs: number;
}

/** The signaling connection behind WebRTC transfers (browsers, other networks). */
export interface SignalingStatus {
  /** null when no signaling server is set up. */
  url: string | null;
  state: "off" | "connecting" | "open" | "closed";
  error: string | null;
  identityKey: string;
}

export type EngineEvent =
  | { type: "localDeviceChanged"; device: LocalDevice }
  | { type: "deviceUpdated"; device: DeviceSummary }
  | { type: "deviceRemoved"; id: string }
  | { type: "incomingRequest"; request: IncomingRequest }
  | { type: "incomingRequestClosed"; id: string; reason: string }
  | { type: "transferUpdated"; transfer: TransferSummary }
  | { type: "transferFilesUpdated"; id: string; files: TransferFile[] }
  | { type: "transferRemoved"; id: string }
  | { type: "historyAdded"; entry: HistoryEntry }
  | { type: "serverStatus"; running: boolean; port: number; error: string | null }
  | { type: "notice"; level: NoticeLevel; code: string; message: string }
  | { type: "browserShareUpdated"; share: BrowserLink }
  | { type: "browserShareRemoved"; id: string }
  | { type: "pairingOfferClosed"; id: string; device: DeviceSummary | null }
  | { type: "pairingRequest"; request: PairingRequest }
  | { type: "pairingRequestClosed"; id: string }
  | { type: "pairingFinished"; id: string; outcome: PairingOutcome; device: DeviceSummary | null; error: ErrorInfo | null }
  | { type: "roomUpdated"; room: RoomInfo }
  | { type: "roomRemoved"; id: string }
  | { type: "signalingStatus"; status: SignalingStatus };

/** Something the user picked to send. */
export type OutgoingItem =
  | { kind: "path"; path: string; name: string; size: number | null; isDir: boolean }
  | { kind: "file"; file: File; name: string; size: number; relativePath?: string }
  /** A browser folder: its files with `/`-separated paths starting at the folder name. */
  | { kind: "folder"; name: string; size: number; files: { file: File; path: string }[] }
  | { kind: "text"; text: string; name: string };

export type SendTarget = { kind: "device"; id: string };

export interface Snapshot {
  local: LocalDevice;
  devices: DeviceSummary[];
  transfers: TransferSummary[];
  settings: Settings;
  server: { running: boolean; port: number; error: string | null };
}

export interface DiagnosticCheck {
  id: string;
  label: string;
  status: "ok" | "warning" | "error" | "unknown";
  value: string;
  detail?: string;
}

/** What this runtime can and cannot do (shown honestly in the UI). */
export interface Capabilities {
  kind: "native" | "web" | "demo";
  lanDiscovery: boolean;
  receiveInBackground: boolean;
  pickFolders: boolean;
  revealInFolder: boolean;
  clipboardRead: boolean;
  nativeNotifications: boolean;
  tray: boolean;
  localsendInterop: boolean;
  /** Can host a plain-HTTP page for browsers on the same network. */
  browserLinks: boolean;
  /** Can pair "my devices" over verified LAN connections. */
  pairing: boolean;
  /** Can open private links (rooms) through a signaling server. */
  remoteLinks: boolean;
}

export interface Platform {
  readonly capabilities: Capabilities;
  init(): Promise<Snapshot>;
  subscribe(handler: (event: EngineEvent) => void): () => void;

  send(targets: SendTarget[], items: OutgoingItem[]): Promise<string[]>;
  respond(requestId: string, decision: Decision): Promise<boolean>;
  cancel(transferId: string): Promise<boolean>;
  pause(transferId: string): Promise<boolean>;
  resume(transferId: string): Promise<boolean>;
  submitPin(transferId: string, pin: string | null): Promise<boolean>;
  dismiss(transferId: string): Promise<boolean>;
  transferFiles(transferId: string): Promise<TransferFile[]>;

  refreshDevices(): Promise<void>;
  addDevice(host: string, port: number): Promise<DeviceSummary>;
  setDeviceFlags(
    id: string,
    flags: { trusted?: boolean; favorite?: boolean; mine?: boolean; customAlias?: string | null },
  ): Promise<DeviceSummary | null>;
  forgetDevice(id: string): Promise<void>;

  /** Pairing ("my devices"). */
  createPairingOffer(): Promise<PairingOffer>;
  cancelPairingOffer(id: string): Promise<boolean>;
  pairWithUri(uri: string): Promise<DeviceSummary>;
  startCodePairing(deviceId: string): Promise<OutgoingPairing>;
  cancelCodePairing(id: string): Promise<boolean>;
  respondPairing(requestId: string, accept: boolean): Promise<boolean>;
  unpairDevice(id: string): Promise<DeviceSummary | null>;

  history(limit: number, beforeId?: number, direction?: Direction): Promise<HistoryEntry[]>;
  deleteHistory(id: number): Promise<boolean>;
  clearHistory(): Promise<void>;

  updateSettings(settings: Settings): Promise<Settings>;

  /** WebRTC through a signaling server: status and private links ("rooms"). */
  signalingStatus(): Promise<SignalingStatus | null>;
  createRoom(): Promise<RoomInfo>;
  joinRoom(link: string): Promise<RoomInfo>;
  leaveRoom(id: string): Promise<boolean>;
  rooms(): Promise<RoomInfo[]>;

  /** Browser links (LAN, no app needed on the other side). */
  shareWithBrowsers(items: OutgoingItem[], pin: string | null): Promise<BrowserLink>;
  receiveFromBrowsers(pin: string | null): Promise<BrowserLink>;
  stopBrowserLink(id: string): Promise<boolean>;
  browserLinks(): Promise<BrowserLink[]>;
  diagnostics(): Promise<DiagnosticCheck[]>;

  /** Pickers; return null when cancelled. */
  pickFiles(): Promise<OutgoingItem[] | null>;
  pickFolder(): Promise<OutgoingItem[] | null>;
  pickSaveFolder(): Promise<string | null>;
  readClipboard(): Promise<OutgoingItem | null>;
  copyText(text: string): Promise<void>;
  open(path: string): Promise<void>;
  reveal(path: string): Promise<void>;
  /** A URL the UI can use to preview a received image. */
  previewUrl(path: string): string | null;
  notify(title: string, body: string): Promise<void>;
}
