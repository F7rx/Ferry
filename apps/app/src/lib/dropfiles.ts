// Browser files and folders → outgoing items. Folders (dropped, or picked
// with `webkitdirectory`) become one `folder` item that keeps the structure
// as relative paths, instead of one chip per file.
import type { OutgoingItem } from "../platform";

/** Stop walking huge trees; the transfer protocol caps files per offer anyway. */
const MAX_FILES = 10_000;

type FolderFile = { file: File; path: string };

/** Reads a drop. Entries must be taken synchronously, so call this inside the drop handler. */
export function itemsFromDataTransfer(dt: DataTransfer): Promise<OutgoingItem[]> {
  const entries = [...dt.items].filter((i) => i.kind === "file").map((i) => i.webkitGetAsEntry?.() ?? null);
  const files = [...dt.files];
  if (!entries.some((e) => e?.isDirectory)) return Promise.resolve(files.map(fileItem));
  return (async () => {
    const out: OutgoingItem[] = [];
    for (const [i, entry] of entries.entries()) {
      if (entry?.isDirectory) {
        const collected: FolderFile[] = [];
        await walk(entry as FileSystemDirectoryEntry, entry.name, collected);
        out.push(folderItem(entry.name, collected));
      } else if (entry?.isFile) {
        out.push(fileItem(await fileOf(entry as FileSystemFileEntry)));
      } else if (files[i]) {
        out.push(fileItem(files[i]));
      }
    }
    return out;
  })();
}

/** Files from `<input type=file>`; with `webkitdirectory` they're grouped by top folder. */
export function itemsFromFileList(list: FileList | File[]): OutgoingItem[] {
  const files = [...list];
  const groups = new Map<string, FolderFile[]>();
  const loose: OutgoingItem[] = [];
  for (const file of files.slice(0, MAX_FILES)) {
    const rel = file.webkitRelativePath;
    if (!rel || !rel.includes("/")) {
      loose.push(fileItem(file));
      continue;
    }
    const top = rel.slice(0, rel.indexOf("/"));
    const group = groups.get(top) ?? [];
    group.push({ file, path: rel });
    groups.set(top, group);
  }
  return [...[...groups].map(([name, entries]) => folderItem(name, entries)), ...loose];
}

function fileItem(file: File): OutgoingItem {
  return { kind: "file", file, name: file.name, size: file.size };
}

function folderItem(name: string, files: FolderFile[]): OutgoingItem {
  return { kind: "folder", name, files, size: files.reduce((n, f) => n + f.file.size, 0) };
}

function fileOf(entry: FileSystemFileEntry): Promise<File> {
  return new Promise((resolve, reject) => entry.file(resolve, reject));
}

async function walk(dir: FileSystemDirectoryEntry, prefix: string, out: FolderFile[]): Promise<void> {
  const reader = dir.createReader();
  for (;;) {
    // readEntries returns batches (about 100 in Chromium) until an empty one.
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => reader.readEntries(resolve, reject));
    if (!batch.length) return;
    for (const entry of batch) {
      if (out.length >= MAX_FILES) return;
      const path = `${prefix}/${entry.name}`;
      if (entry.isFile) out.push({ file: await fileOf(entry as FileSystemFileEntry), path });
      else if (entry.isDirectory) await walk(entry as FileSystemDirectoryEntry, path, out);
    }
  }
}
