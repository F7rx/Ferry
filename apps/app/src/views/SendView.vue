<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useRoute } from "vue-router";
import { ArrowRight, Check, ClipboardPaste, FilePlus2, FolderPlus, Plus, Search, Type } from "@lucide/vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import FileChip from "../components/FileChip.vue";
import DeviceAvatar from "../components/DeviceAvatar.vue";
import { compose, sendStaged, stage, stagedBytes, toggleTarget, unstage } from "../stores/compose";
import { attempt, devices, displayName, toast } from "../stores/engine";
import { platform } from "../platform";
import { formatBytes, plural } from "../lib/format";

const route = useRoute();
onMounted(() => {
  const to = route.query.to;
  if (typeof to === "string") compose.targets.add(to);
});

const text = ref("");
const query = ref("");
const visible = computed(() => {
  const q = query.value.trim().toLowerCase();
  return devices.value.filter((d) => !q || displayName(d).toLowerCase().includes(q) || (d.address ?? "").includes(q));
});

async function add(kind: "files" | "folder" | "paste") {
  const items = kind === "files" ? await platform.pickFiles() : kind === "folder" ? await platform.pickFolder() : [await platform.readClipboard()].filter(Boolean);
  if (items?.length) stage(items as never);
}
function addText() {
  const v = text.value.trim();
  if (!v) return;
  stage([{ kind: "text", text: v, name: "Text" }]);
  text.value = "";
}

const host = ref("");
const port = ref(53317);
const adding = ref(false);
async function addByAddress() {
  if (!host.value.trim()) return;
  adding.value = true;
  const d = await attempt(() => platform.addDevice(host.value.trim(), Number(port.value) || 53317));
  adding.value = false;
  if (d) {
    compose.targets.add(d.id);
    host.value = "";
    toast({ level: "success", title: `Found ${displayName(d)}` }, 2500);
  }
}

const sending = ref(false);
async function send() {
  sending.value = true;
  await sendStaged();
  sending.value = false;
}
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Send</h1>
        <p>Pick what to send and who gets it. Several devices at once is fine.</p>
      </div>
    </header>

    <div class="cols">
      <section class="module glass" aria-labelledby="what">
        <div class="module-head">
          <h2 id="what">What</h2>
          <span class="sub tabular">{{ compose.items.length ? `${plural(compose.items.length, "item")} · ${formatBytes(stagedBytes)}` : "" }}</span>
        </div>
        <TransitionGroup v-if="compose.items.length" tag="div" name="list" class="items">
          <FileChip v-for="item in compose.items" :key="item.key" :item="item" @remove="unstage(item.key)" />
        </TransitionGroup>
        <p v-else class="hint">Drop files anywhere in this window, or add them here.</p>
        <div class="adders">
          <FButton :icon="FilePlus2" @click="add('files')">Files</FButton>
          <FButton :icon="FolderPlus" @click="add('folder')">Folder</FButton>
          <FButton :icon="ClipboardPaste" @click="add('paste')">Paste</FButton>
        </div>
        <form class="text" @submit.prevent="addText">
          <label for="send-text" class="eyebrow"><FIcon :icon="Type" :size="12" /> Text or link</label>
          <div class="text-row">
            <input id="send-text" v-model="text" class="input" placeholder="Type a note or paste a link" />
            <FButton type="submit" :icon="Plus" :disabled="!text.trim()">Add</FButton>
          </div>
        </form>
      </section>

      <section class="module glass" aria-labelledby="who">
        <div class="module-head">
          <h2 id="who">To</h2>
          <span class="sub">{{ compose.targets.size ? `${compose.targets.size} selected` : "" }}</span>
        </div>
        <label class="search">
          <FIcon :icon="Search" :size="16" />
          <input v-model="query" placeholder="Search devices" aria-label="Search devices" />
        </label>
        <ul class="row-list devices">
          <li v-for="d in visible" :key="d.id">
            <button type="button" class="device" :class="{ on: compose.targets.has(d.id) }" :aria-pressed="compose.targets.has(d.id)" :data-device-id="d.id" @click="toggleTarget(d.id)">
              <DeviceAvatar :kind="d.deviceKind" :model="d.deviceModel" :size="36" :online="d.online" />
              <span class="dtext">
                <strong>{{ displayName(d) }}</strong>
                <span>{{ d.online ? (d.verified ? (d.isFerry ? "Ferry" : "LocalSend") : "Not encrypted") : "Offline" }}{{ d.address ? ` · ${d.address}` : "" }}</span>
              </span>
              <span class="tick"><FIcon :icon="Check" :size="14" :stroke="3" /></span>
            </button>
          </li>
          <li v-if="!visible.length" class="none">No devices found yet.</li>
        </ul>
        <form class="by-address" @submit.prevent="addByAddress">
          <span class="eyebrow">Add by address</span>
          <div class="text-row">
            <input v-model="host" class="input" placeholder="192.168.1.20" aria-label="IP address" inputmode="decimal" />
            <input v-model.number="port" class="input port" aria-label="Port" inputmode="numeric" />
            <FButton type="submit" :disabled="!host.trim() || adding">{{ adding ? "Looking…" : "Find" }}</FButton>
          </div>
        </form>
      </section>
    </div>

    <div class="send-row">
      <FButton variant="primary" size="lg" :icon="ArrowRight" :disabled="!compose.items.length || !compose.targets.size || sending" @click="send">
        {{ compose.targets.size > 1 ? `Send to ${compose.targets.size} devices` : "Send" }}
      </FButton>
    </div>
  </div>
</template>

<style scoped>
.cols {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: var(--space-5);
  align-items: start;
}
@media (max-width: 899px) {
  .cols {
    grid-template-columns: minmax(0, 1fr);
  }
}
.items {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.hint {
  padding: var(--space-6) 0;
  text-align: center;
  color: var(--text-3);
  border: 1.5px dashed var(--hairline-strong);
  border-radius: var(--radius-lg);
}
.adders {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  margin-top: var(--space-4);
}
.text {
  margin-top: var(--space-5);
}
.text .eyebrow,
.by-address .eyebrow {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 6px;
}
.text-row {
  display: flex;
  gap: var(--space-2);
}
.text-row .input {
  flex: 1;
  min-width: 0;
}
.text-row .port {
  flex: none;
  width: 84px;
}
.search {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 12px;
  margin-bottom: var(--space-2);
  border-radius: var(--radius-pill);
  background: var(--fill-1);
  color: var(--text-3);
}
.search input {
  flex: 1;
  border: 0;
  background: none;
  font-size: var(--text-md);
}
.search input:focus {
  outline: none;
}
.devices {
  max-height: 420px;
  overflow: auto;
}
.device {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  min-height: 56px;
  padding: 8px;
  border: 0;
  border-radius: var(--radius-md);
  background: none;
  text-align: left;
}
@media (hover: hover) {
  .device:hover {
    background: var(--fill-hover);
  }
}
.dtext {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}
.dtext strong {
  font-size: var(--text-md);
  font-weight: 600;
}
.dtext span {
  overflow: hidden;
  font-size: var(--text-xs);
  color: var(--text-3);
  white-space: nowrap;
  text-overflow: ellipsis;
}
.tick {
  display: grid;
  place-items: center;
  width: 22px;
  height: 22px;
  border-radius: 50%;
  color: transparent;
  box-shadow: inset 0 0 0 1.5px var(--hairline-strong);
  transition:
    background-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out);
}
.device.on .tick {
  color: var(--text-on-accent);
  background: var(--accent);
  box-shadow: none;
}
.none {
  padding: var(--space-5) 0;
  text-align: center;
  color: var(--text-3);
}
.by-address {
  margin-top: var(--space-4);
}
.send-row {
  display: flex;
  justify-content: flex-end;
}
.list-enter-active,
.list-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--dur-small) var(--ease-out);
}
.list-enter-from,
.list-leave-to {
  opacity: 0;
  transform: scale(calc(1 - 0.06 * var(--motion-scale)));
}
</style>
