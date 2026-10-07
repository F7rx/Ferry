// Native (Tauri) platform: the Rust engine runs in-process; we talk to it over IPC.
import { reactive } from "vue";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import type {
  Capabilities,
  Decision,
  DeviceSummary,
  DiagnosticCheck,
  Direction,
  EngineEvent,
  HistoryEntry,
  OutgoingItem,
  Platform,
  SendTarget,
  Settings,
  Snapshot,
  TransferFile,
  BrowserLink,
  OutgoingPairing,
  PairingOffer,
  RoomInfo,
  SignalingStatus,
} from "./types";

interface PathInfo {
  path: string;
  name: string;
  size: number | null;
  isDir: boolean;
}

export interface NativeDragEvent {
  type: "enter" | "over" | "drop" | "leave";
  paths: string[];
  x: number;
  y: number;
}

async function pathItems(paths: string[]): Promise<OutgoingItem[]> {
  if (paths.length === 0) return [];
  const infos = await invoke<PathInfo[]>("path_info", { paths });
  return infos.map((i) => ({ kind: "path", path: i.path, name: i.name, size: i.size, isDir: i.isDir }));
}

export function createNativePlatform(): Platform & {
  takePendingPaths(): Promise<string[]>;
  onDrag(handler: (e: NativeDragEvent) => void): () => void;
  onPaths(handler: (items: OutgoingItem[]) => void): () => void;
  pathItems: typeof pathItems;
} {
  const capabilities: Capabilities = {
    kind: "native",
    lanDiscovery: true,
    receiveInBackground: true,
    pickFolders: true,
    revealInFolder: true,
    clipboardRead: true,
    nativeNotifications: true,
    tray: true,
    localsendInterop: true,
    browserLinks: true,
    pairing: true,
    remoteLinks: true,
  };

  // Received files the shell granted to the asset protocol (as history spells
  // them). Reactive, so a preview appears once its grant arrives.
  const previewable = reactive(new Set<string>());
  const allowPreviews = (paths: string[]) => paths.forEach((p) => previewable.add(p));

  // Launch paths wait in the shell's queue until taken: first by the startup
  // handoff (main.ts), then by `onPaths` whenever the shell announces more and
  // once its listener is up (a launch may beat the window). Taking, not the
  // event's payload, delivers them, so none is lost and none arrives twice.
  let handOff!: () => void;
  const handedOff = new Promise<void>((resolve) => (handOff = resolve));

  return {
    capabilities,
    pathItems,

    init: () => invoke<Snapshot>("snapshot"),

    /** Launch paths ("Send with Ferry", command line): handed over once. */
    async takePendingPaths() {
      try {
        return await invoke<string[]>("take_pending_paths");
      } finally {
        handOff();
      }
    },

    async subscribe(handler, onResync) {
      // All listeners are installed before this resolves, so the snapshot
      // that follows can't miss an event.
      const installed = await Promise.allSettled([
        listen<EngineEvent>("ferry://event", (e) => handler(e.payload)),
        listen<null>("ferry://resync", () => onResync?.()),
        listen<string[]>("ferry://previewable", (e) => allowPreviews(e.payload)),
      ]);
      const unlisten = installed.flatMap((r) => (r.status === "fulfilled" ? [r.value] : []));
      const failed = installed.find((r) => r.status === "rejected");
      const stop = () => unlisten.splice(0).forEach((u: UnlistenFn) => u());
      if (failed) {
        stop();
        throw failed.reason;
      }
      return stop;
    },

    onDrag(handler) {
      let unlisten: UnlistenFn | null = null;
      let disposed = false;
      getCurrentWebview()
        .onDragDropEvent((event) => {
          const p = event.payload as { type: string; paths?: string[]; position?: { x: number; y: number } };
          const scale = window.devicePixelRatio || 1;
          const x = (p.position?.x ?? 0) / scale;
          const y = (p.position?.y ?? 0) / scale;
          const type = p.type === "enter" ? "enter" : p.type === "over" ? "over" : p.type === "drop" ? "drop" : "leave";
          handler({ type, paths: p.paths ?? [], x, y });
        })
        .then((u) => {
          if (disposed) u();
          else unlisten = u;
        });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },

    onPaths(handler) {
      let unlisten: UnlistenFn | null = null;
      let disposed = false;
      const drain = async () => {
        // The startup handoff takes what launched the app first.
        await handedOff;
        if (disposed) return;
        const paths = await invoke<string[]>("take_pending_paths");
        if (paths.length) handler(await pathItems(paths));
      };
      const take = () => void drain().catch((err) => console.error("Couldn't take the files Ferry was opened with", err));
      listen<string[]>("ferry://paths", take).then((u) => {
        if (disposed) return u();
        unlisten = u;
        take();
      });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },

    async send(targets: SendTarget[], items: OutgoingItem[]) {
      const dto = items.map((i) => {
        if (i.kind === "path") return { kind: "path", path: i.path };
        if (i.kind === "text") return { kind: "text", text: i.text };
        throw new Error("Browser files can't be sent from the desktop app");
      });
      return invoke<string[]>("send", { targets, items: dto });
    },
    respond: (requestId: string, decision: Decision) => invoke<boolean>("respond", { requestId, decision }),
    cancel: (id) => invoke<boolean>("cancel", { id }),
    pause: (id) => invoke<boolean>("pause", { id }),
    resume: (id) => invoke<boolean>("resume", { id }),
    submitPin: (id, pin) => invoke<boolean>("submit_pin", { id, pin }),
    dismiss: (id) => invoke<boolean>("dismiss", { id }),
    transferFiles: (id) => invoke<TransferFile[]>("transfer_files", { id }),

    refreshDevices: () => invoke<void>("refresh_devices"),
    addDevice: (host, port) => invoke<DeviceSummary>("add_device", { host, port }),
    setDeviceFlags: (id, flags) => invoke<DeviceSummary | null>("set_device_flags", { id, flags }),
    forgetDevice: (id) => invoke<void>("forget_device", { id }),
    signalingStatus: () => invoke<SignalingStatus>("signaling_status"),
    createRoom: () => invoke<RoomInfo>("create_room"),
    joinRoom: (link) => invoke<RoomInfo>("join_room", { link }),
    leaveRoom: (id) => invoke<boolean>("leave_room", { id }),
    rooms: () => invoke<RoomInfo[]>("rooms"),
    createPairingOffer: () => invoke<PairingOffer>("create_pairing_offer"),
    cancelPairingOffer: (id) => invoke<boolean>("cancel_pairing_offer", { id }),
    pairWithUri: (uri) => invoke<DeviceSummary>("pair_with_uri", { uri }),
    startCodePairing: (deviceId) => invoke<OutgoingPairing>("start_code_pairing", { deviceId }),
    cancelCodePairing: (id) => invoke<boolean>("cancel_code_pairing", { id }),
    respondPairing: (requestId, accept) => invoke<boolean>("respond_pairing", { requestId, accept }),
    unpairDevice: (id) => invoke<DeviceSummary | null>("unpair_device", { id }),

    async history(limit, beforeId, direction?: Direction) {
      const page = await invoke<{ entries: HistoryEntry[]; previewable: string[] }>("history", {
        limit,
        beforeId: beforeId ?? null,
        direction: direction ?? null,
      });
      allowPreviews(page.previewable);
      return page.entries;
    },
    deleteHistory: (id) => invoke<boolean>("delete_history", { id }),
    clearHistory: () => invoke<void>("clear_history"),

    updateSettings: (settings: Settings) => invoke<Settings>("update_settings", { settings }),
    shareWithBrowsers: (items, pin) =>
      invoke<BrowserLink>("share_with_browsers", {
        items: items.filter((i) => i.kind === "path").map((i) => ({ kind: "path", path: (i as { path: string }).path })),
        pin,
      }),
    receiveFromBrowsers: (pin) => invoke<BrowserLink>("receive_from_browsers", { pin }),
    stopBrowserLink: (id) => invoke<boolean>("stop_browser_link", { id }),
    browserLinks: () => invoke<BrowserLink[]>("browser_links"),
    diagnostics: () => invoke<DiagnosticCheck[]>("diagnostics"),

    async pickFiles() {
      const picked = await openDialog({ multiple: true, title: "Choose files to send" });
      if (!picked) return null;
      return pathItems(Array.isArray(picked) ? picked : [picked]);
    },
    async pickFolder() {
      const picked = await openDialog({ directory: true, multiple: true, title: "Choose folders to send" });
      if (!picked) return null;
      return pathItems(Array.isArray(picked) ? picked : [picked]);
    },
    async pickSaveFolder() {
      const picked = await openDialog({ directory: true, multiple: false, title: "Save received files to" });
      return typeof picked === "string" ? picked : null;
    },
    async readClipboard() {
      try {
        const text = await readText();
        if (text && text.trim()) return { kind: "text", text, name: "Clipboard" };
      } catch {
        /* empty or not text */
      }
      return null;
    },
    copyText: (text) => writeText(text),
    open: (path) => invoke<void>("open_path", { path }),
    reveal: (path) => invoke<void>("reveal_path", { path }),
    // Only files the shell validated and granted; anything else would be
    // refused by the asset protocol anyway.
    previewUrl: (path) => (previewable.has(path) ? convertFileSrc(path) : null),
    async notify(title, body) {
      let granted = await isPermissionGranted();
      if (!granted) granted = (await requestPermission()) === "granted";
      if (granted) sendNotification({ title, body });
    },
  };
}
