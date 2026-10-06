export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "Unknown";
  if (bytes < 1000) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let v = bytes;
  let u = -1;
  while (v >= 1000 && u < units.length - 1) {
    v /= 1000;
    u++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[u]}`;
}

export function formatSpeed(bps: number): string {
  return bps > 0 ? `${formatBytes(bps)}/s` : "";
}

export function formatEta(secs: number | null | undefined): string {
  if (secs == null || !Number.isFinite(secs)) return "";
  if (secs < 5) return "a few seconds left";
  if (secs < 60) return `${Math.round(secs)} s left`;
  if (secs < 3600) return `${Math.round(secs / 60)} min left`;
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return `${h} h ${m} min left`;
}

const rtf = typeof Intl !== "undefined" ? new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }) : null;

export function relativeTime(ms: number, now = Date.now()): string {
  const diff = (ms - now) / 1000;
  const abs = Math.abs(diff);
  if (abs < 45) return "just now";
  if (!rtf) return new Date(ms).toLocaleString();
  if (abs < 3600) return rtf.format(Math.round(diff / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), "hour");
  if (abs < 7 * 86400) return rtf.format(Math.round(diff / 86400), "day");
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: abs > 300 * 86400 ? "numeric" : undefined });
}

export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function dayLabel(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const yesterday = new Date(Date.now() - 86400000);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" });
}

export type FileKind = "image" | "video" | "audio" | "document" | "archive" | "code" | "text" | "folder" | "app" | "other";

const EXT: Record<string, FileKind> = {
  jpg: "image", jpeg: "image", png: "image", gif: "image", webp: "image", heic: "image", heif: "image", avif: "image", bmp: "image", svg: "image", raw: "image", dng: "image",
  mp4: "video", mov: "video", mkv: "video", webm: "video", avi: "video", m4v: "video",
  mp3: "audio", wav: "audio", flac: "audio", m4a: "audio", aac: "audio", ogg: "audio", opus: "audio",
  pdf: "document", doc: "document", docx: "document", key: "document", pages: "document", ppt: "document", pptx: "document", xls: "document", xlsx: "document", numbers: "document", odt: "document",
  zip: "archive", rar: "archive", "7z": "archive", tar: "archive", gz: "archive", xz: "archive", iso: "archive", dmg: "archive",
  js: "code", ts: "code", rs: "code", py: "code", json: "code", html: "code", css: "code", go: "code", java: "code", kt: "code", swift: "code", c: "code", cpp: "code",
  txt: "text", md: "text", csv: "text", log: "text", rtf: "text",
  exe: "app", msi: "app", apk: "app", appimage: "app", deb: "app", rpm: "app", pkg: "app",
};

export function fileKind(name: string, mime?: string, isDir = false): FileKind {
  if (isDir) return "folder";
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  if (EXT[ext]) return EXT[ext]!;
  if (mime?.startsWith("image/")) return "image";
  if (mime?.startsWith("video/")) return "video";
  if (mime?.startsWith("audio/")) return "audio";
  if (mime?.startsWith("text/")) return "text";
  return "other";
}

export function isUrl(text: string): boolean {
  return /^https?:\/\/\S+$/i.test(text.trim());
}

/** The first http(s) link inside free text, if any. */
export function firstUrl(text: string): string | null {
  const m = text.match(/https?:\/\/[^\s<>"'）)\]]+/i);
  return m ? m[0].replace(/[.,;:!?]+$/, "") : null;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}
