<script setup lang="ts">
// Everything received, with previews and the actions people actually use.
import { computed, onMounted, ref } from "vue";
import { Copy, Download, ExternalLink, FolderOpen, Grid2x2, List, Search, ShieldCheck, Trash2 } from "@lucide/vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import FSegmented from "../components/FSegmented.vue";
import { attempt, store, toast } from "../stores/engine";
import { platform, type HistoryEntry } from "../platform";
import { fileKind, formatBytes, isUrl, relativeTime } from "../lib/format";
import { fileIcon } from "../lib/icons";

type Filter = "all" | "media" | "documents" | "messages" | "other";
const filter = ref<Filter>("all");
const view = ref<"grid" | "list">("grid");
const query = ref("");
const items = ref<HistoryEntry[]>([]);

onMounted(async () => {
  items.value = (await attempt(() => platform.history(400, undefined, "receive"))) ?? [];
});
// New arrivals stream in from engine events.
const all = computed(() => {
  const seen = new Set(items.value.map((i) => i.id));
  const fresh = store.history.filter((h) => h.direction === "receive" && !seen.has(h.id));
  return [...fresh, ...items.value];
});
const visible = computed(() => {
  const q = query.value.trim().toLowerCase();
  return all.value.filter((h) => {
    if (h.status !== "completed") return false;
    if (q && !h.name.toLowerCase().includes(q) && !h.peerAlias.toLowerCase().includes(q)) return false;
    const k = h.kind === "text" ? "text" : fileKind(h.name, h.mime);
    switch (filter.value) {
      case "media":
        return k === "image" || k === "video" || k === "audio";
      case "documents":
        return k === "document" || k === "text";
      case "messages":
        return h.kind === "text";
      case "other":
        return !["image", "video", "audio", "document", "text"].includes(k);
      default:
        return true;
    }
  });
});

const filters: { value: Filter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "media", label: "Photos & media" },
  { value: "documents", label: "Documents" },
  { value: "messages", label: "Messages" },
  { value: "other", label: "Other" },
];

function preview(h: HistoryEntry) {
  if (!h.path || fileKind(h.name, h.mime) !== "image") return null;
  return platform.previewUrl(h.path);
}
async function remove(h: HistoryEntry) {
  if (await attempt(() => platform.deleteHistory(h.id))) {
    items.value = items.value.filter((i) => i.id !== h.id);
    const i = store.history.findIndex((x) => x.id === h.id);
    if (i >= 0) store.history.splice(i, 1);
  }
}
const canReveal = platform.capabilities.revealInFolder;
// In the browser, received files live in private storage: "Save" downloads a copy.
const isWeb = platform.capabilities.kind === "web";
const openFile = (h: HistoryEntry) => h.path && attempt(() => platform.open(h.path!));
const revealFile = (h: HistoryEntry) => h.path && attempt(() => platform.reveal(h.path!));
function openLink(url: string) {
  if (isUrl(url)) window.open(url, "_blank", "noopener,noreferrer");
}
async function copy(h: HistoryEntry) {
  await platform.copyText(h.text ?? h.path ?? h.name);
  toast({ level: "success", title: h.text ? "Text copied" : "Path copied" }, 1500);
}
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Inbox</h1>
        <p>Everything you've received, in one place.</p>
      </div>
      <div class="page-actions">
        <label class="search">
          <FIcon :icon="Search" :size="16" />
          <input v-model="query" placeholder="Search" aria-label="Search inbox" />
        </label>
        <FSegmented
          :model-value="view"
          :options="[
            { value: 'grid', label: 'Grid' },
            { value: 'list', label: 'List' },
          ]"
          label="View"
          @update:model-value="(v) => (view = v)"
        />
      </div>
    </header>

    <FSegmented :model-value="filter" :options="filters" label="Filter" class="filters" @update:model-value="(v) => (filter = v)" />

    <div v-if="!visible.length" class="module glass empty">
      <strong>{{ all.length ? "Nothing matches" : "Nothing here yet" }}</strong>
      <span>{{ all.length ? "Try another filter." : "Files and messages you receive will appear here." }}</span>
    </div>

    <TransitionGroup v-else tag="ul" name="inbox" class="items" :class="view">
      <li v-for="h in visible" :key="h.id" class="item glass">
        <div class="thumb">
          <img v-if="preview(h)" :src="preview(h)!" alt="" loading="lazy" decoding="async" />
          <FIcon v-else :icon="fileIcon(h.name, h.mime)" :size="view === 'grid' ? 30 : 18" :stroke="1.5" />
        </div>
        <div class="text">
          <strong class="name" :title="h.name">{{ h.kind === "text" && h.text ? h.text : h.name }}</strong>
          <span class="meta tabular">
            {{ h.kind === "text" ? "Message" : formatBytes(h.size) }} · {{ h.peerAlias }}
            <FIcon v-if="h.verified" :icon="ShieldCheck" :size="12" class="ok" title="Checksum verified end to end" />
          </span>
          <span class="meta when">{{ relativeTime(h.timestampMs) }}</span>
        </div>
        <div class="actions">
          <FButton v-if="h.path" variant="ghost" size="sm" icon-only :icon="ExternalLink" label="Open" @click="openFile(h)" />
          <FButton v-if="h.path && canReveal" variant="ghost" size="sm" icon-only :icon="FolderOpen" label="Show in folder" @click="revealFile(h)" />
          <FButton v-if="h.path && isWeb" variant="ghost" size="sm" icon-only :icon="Download" :label="`Save ${h.name.split('/').pop()}`" @click="revealFile(h)" />
          <FButton v-if="h.text && isUrl(h.text)" variant="ghost" size="sm" icon-only :icon="ExternalLink" label="Open link" @click="openLink(h.text!)" />
          <FButton v-if="h.text || !isWeb" variant="ghost" size="sm" icon-only :icon="Copy" :label="h.text ? 'Copy text' : 'Copy path'" @click="copy(h)" />
          <FButton variant="ghost" size="sm" icon-only :icon="Trash2" :label="isWeb && h.path ? 'Delete from this browser' : 'Remove from inbox (keeps the file)'" @click="remove(h)" />
        </div>
      </li>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.search {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 14px;
  border-radius: var(--radius-pill);
  background: var(--fill-1);
  color: var(--text-3);
  box-shadow: inset 0 0 0 1px var(--hairline);
}
.search input {
  width: 160px;
  border: 0;
  background: none;
  font-size: var(--text-md);
}
.search input:focus {
  outline: none;
}
.filters {
  align-self: flex-start;
  max-width: 100%;
  overflow-x: auto;
}
.items {
  display: grid;
  gap: var(--space-3);
  margin: 0;
  padding: 0;
  list-style: none;
}
.items.grid {
  grid-template-columns: repeat(auto-fill, minmax(196px, 1fr));
}
.item {
  position: relative;
  display: flex;
  overflow: hidden;
  border-radius: var(--radius-lg);
}
.grid .item {
  flex-direction: column;
}
.list .item {
  align-items: center;
  gap: var(--space-3);
  padding: 8px 8px 8px 10px;
}
.thumb {
  display: grid;
  place-items: center;
  flex: none;
  color: var(--accent-strong);
  background: linear-gradient(160deg, var(--accent-softer), var(--fill-1));
}
:root[data-theme="dark"] .thumb {
  color: var(--accent-text);
}
.grid .thumb {
  aspect-ratio: 4 / 3;
}
.list .thumb {
  width: 40px;
  height: 40px;
  border-radius: var(--radius-sm);
}
.thumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.text {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
}
.grid .text {
  padding: 10px 12px 12px;
}
.name {
  overflow: hidden;
  font-size: var(--text-sm);
  font-weight: 600;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.meta {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  overflow: hidden;
  font-size: var(--text-xs);
  color: var(--text-3);
  white-space: nowrap;
}
.ok {
  color: var(--ok);
}
.actions {
  display: flex;
  gap: 2px;
}
.grid .actions {
  position: absolute;
  top: 8px;
  right: 8px;
  padding: 3px;
  border-radius: var(--radius-pill);
  background: var(--glass-2);
  box-shadow: var(--shadow-1);
  opacity: 0;
  transform: translateY(calc(-4px * var(--motion-scale)));
  transition:
    opacity var(--dur-control) var(--ease-out),
    transform var(--dur-control) var(--ease-out);
}
.grid .item:hover .actions,
.grid .item:focus-within .actions {
  opacity: 1;
  transform: none;
}
@media (hover: none) {
  .grid .actions {
    opacity: 1;
    transform: none;
  }
}
.inbox-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-settle-dur) var(--spring-settle);
}
.inbox-leave-active {
  transition: opacity var(--dur-small) var(--ease-in);
}
.inbox-enter-from {
  opacity: 0;
  transform: translateY(calc(10px * var(--motion-scale))) scale(calc(1 - 0.04 * var(--motion-scale)));
}
.inbox-leave-to {
  opacity: 0;
}
</style>
