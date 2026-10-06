// Icon choices for devices and files.
import {
  Film,
  File,
  FileArchive,
  FileCode,
  FileText,
  Folder,
  Globe,
  Image,
  Laptop,
  Monitor,
  Music,
  Package,
  Server,
  Smartphone,
  Tablet,
  Terminal,
  Type,
} from "@lucide/vue";
import type { Component } from "vue";
import type { DeviceKind } from "../platform";
import { fileKind, type FileKind } from "./format";

export function deviceIcon(kind: DeviceKind, model?: string | null): Component {
  const m = (model ?? "").toLowerCase();
  if (m.includes("ipad") || m.includes("tablet")) return Tablet;
  if (kind === "mobile") return Smartphone;
  if (kind === "web") return Globe;
  if (kind === "server") return Server;
  if (kind === "headless") return Terminal;
  if (m.includes("mac") || m.includes("laptop")) return Laptop;
  return Monitor;
}

const FILE_ICONS: Record<FileKind, Component> = {
  image: Image,
  video: Film,
  audio: Music,
  document: FileText,
  archive: FileArchive,
  code: FileCode,
  text: Type,
  folder: Folder,
  app: Package,
  other: File,
};

export function fileIcon(name: string, mime?: string, isDir = false): Component {
  return FILE_ICONS[fileKind(name, mime, isDir)];
}
