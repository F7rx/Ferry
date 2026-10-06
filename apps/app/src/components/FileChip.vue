<script setup lang="ts">
import { computed } from "vue";
import { X } from "@lucide/vue";
import type { OutgoingItem } from "../platform";
import { fileIcon } from "../lib/icons";
import { formatBytes } from "../lib/format";
import FIcon from "./FIcon.vue";

const props = defineProps<{ item: OutgoingItem }>();
defineEmits<{ remove: [] }>();

const icon = computed(() => {
  const i = props.item;
  return fileIcon(i.name, undefined, (i.kind === "path" && i.isDir) || i.kind === "folder");
});
const detail = computed(() => {
  const i = props.item;
  if (i.kind === "text") return i.text.length > 60 ? `${i.text.slice(0, 60)}…` : i.text;
  if (i.kind === "path" && i.isDir) return "Folder";
  if (i.kind === "folder") return `${i.files.length} ${i.files.length === 1 ? "file" : "files"} · ${formatBytes(i.size)}`;
  const size = i.kind === "path" ? i.size : i.size;
  return size == null ? "" : formatBytes(size);
});
const title = computed(() => (props.item.kind === "text" ? "Text" : props.item.name));
</script>

<template>
  <div class="chip">
    <span class="icon"><FIcon :icon="icon" :size="16" /></span>
    <span class="text">
      <span class="name">{{ title }}</span>
      <span class="detail tabular">{{ detail }}</span>
    </span>
    <button type="button" class="remove" :aria-label="`Remove ${title}`" @click="$emit('remove')">
      <FIcon :icon="X" :size="14" />
    </button>
  </div>
</template>

<style scoped>
.chip {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
  max-width: 260px;
  height: 52px;
  padding: 0 6px 0 8px;
  border-radius: var(--radius-md);
  background: var(--solid-1);
  box-shadow:
    inset 0 0 0 1px var(--hairline),
    var(--shadow-1);
}
.icon {
  display: grid;
  place-items: center;
  flex: none;
  width: 36px;
  height: 36px;
  border-radius: var(--radius-sm);
  color: var(--accent-strong);
  background: var(--accent-softer);
}
:root[data-theme="dark"] .icon {
  color: var(--accent-text);
}
.text {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.25;
}
.name,
.detail {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.name {
  font-size: var(--text-sm);
  font-weight: 560;
}
.detail {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.remove {
  display: grid;
  place-items: center;
  flex: none;
  width: 28px;
  height: 28px;
  margin-left: auto;
  border: 0;
  border-radius: 50%;
  color: var(--text-3);
  background: transparent;
  transition:
    background-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out);
}
@media (hover: hover) {
  .remove:hover {
    color: var(--text-1);
    background: var(--fill-hover);
  }
}
</style>
