<script setup lang="ts">
import { computed, ref } from "vue";
import { Handshake, LockOpen, Pencil, Plus, RefreshCw, ShieldCheck, ShieldOff, Star, Trash2, Unlink, X } from "@lucide/vue";
import DeviceAvatar from "../components/DeviceAvatar.vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import PairPanel from "../components/PairPanel.vue";
import { cancelCompare, canPair, startCompare, unpair } from "../stores/pairing";
import { attempt, devices, displayName, setFlags, store, toast } from "../stores/engine";
import { platform, type DeviceSummary } from "../platform";
import { relativeTime } from "../lib/format";

const sections = computed(() => [
  { id: "mine", title: "My devices", hint: "Paired with this device. Files arrive without asking.", items: devices.value.filter((d) => d.mine) },
  { id: "trusted", title: "Trusted", hint: "Verified devices you chose to trust.", items: devices.value.filter((d) => d.trusted && !d.mine) },
  { id: "nearby", title: "Nearby", hint: "On this network right now.", items: devices.value.filter((d) => d.online && !d.trusted && !d.mine) },
  { id: "other", title: "Remembered", hint: "Favorites that are currently offline.", items: devices.value.filter((d) => !d.online && !d.trusted && !d.mine) },
]);

const renaming = ref<string | null>(null);
const draft = ref("");
function startRename(d: DeviceSummary) {
  renaming.value = d.id;
  draft.value = displayName(d);
}
async function commitRename(d: DeviceSummary) {
  const name = draft.value.trim();
  renaming.value = null;
  await setFlags(d.id, { customAlias: name && name !== d.alias ? name : null });
}
async function forget(d: DeviceSummary) {
  await attempt(() => platform.forgetDevice(d.id));
  store.devices.delete(d.id);
}

const host = ref("");
const port = ref(53317);
const busy = ref(false);
async function add() {
  busy.value = true;
  const d = await attempt(() => platform.addDevice(host.value.trim(), Number(port.value) || 53317));
  busy.value = false;
  if (d) {
    store.devices.set(d.id, d);
    host.value = "";
    toast({ level: "success", title: `Added ${displayName(d)}` }, 2500);
  }
}
const refreshing = ref(false);
async function refresh() {
  refreshing.value = true;
  await attempt(() => platform.refreshDevices());
  setTimeout(() => (refreshing.value = false), 1500);
}
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Devices</h1>
        <p>Who you can send to, and who you trust.</p>
      </div>
      <div class="page-actions">
        <FButton :icon="RefreshCw" :disabled="refreshing" @click="refresh">{{ refreshing ? "Looking…" : "Look again" }}</FButton>
      </div>
    </header>

    <template v-for="s in sections" :key="s.id">
      <section v-if="s.items.length" class="module glass" :aria-labelledby="`sec-${s.id}`">
        <div class="module-head">
          <div>
            <h2 :id="`sec-${s.id}`">{{ s.title }}</h2>
            <p class="sub">{{ s.hint }}</p>
          </div>
        </div>
        <ul class="row-list">
          <template v-for="d in s.items" :key="d.id">
          <li class="device">
            <DeviceAvatar :kind="d.deviceKind" :model="d.deviceModel" :size="42" :online="d.online" />
            <div class="text">
              <input v-if="renaming === d.id" v-model="draft" class="input rename" :aria-label="`Name for ${d.alias}`" @keydown.enter="commitRename(d)" @keydown.esc="renaming = null" @blur="commitRename(d)" />
              <strong v-else>{{ displayName(d) }}</strong>
              <span>
                <template v-if="d.customAlias">{{ d.alias }} · </template>
                {{ d.deviceModel ?? d.deviceKind }} ·
                <template v-if="d.online">{{ d.address }}<template v-if="d.rttMs != null"> · {{ d.rttMs }} ms</template></template>
                <template v-else>seen {{ relativeTime(d.lastSeenMs) }}</template>
              </span>
              <span class="badges">
                <span v-if="!d.verified" class="badge warn"><FIcon :icon="LockOpen" :size="11" /> Not encrypted</span>
                <span v-else class="badge"><FIcon :icon="ShieldCheck" :size="11" /> Verified identity</span>
                <span class="badge">{{ d.isFerry ? "Ferry" : "LocalSend" }}</span>
              </span>
            </div>
            <div class="actions">
              <FButton variant="ghost" size="sm" icon-only :icon="Star" :label="d.favorite ? 'Remove from favorites' : 'Add to favorites'" :class="{ starred: d.favorite }" :disabled="!d.verified" @click="setFlags(d.id, { favorite: !d.favorite })" />
              <FButton v-if="canPair(d) && store.pairing.outgoing?.peer.id !== d.id" size="sm" :icon="Handshake" :disabled="!!store.pairing.outgoing" @click="startCompare(d)">Pair</FButton>
              <FButton v-if="d.mine" variant="ghost" size="sm" icon-only :icon="Unlink" label="Remove from my devices" @click="unpair(d)" />
              <FButton v-else-if="d.verified" variant="ghost" size="sm" icon-only :icon="d.trusted ? ShieldOff : ShieldCheck" :label="d.trusted ? 'Stop trusting' : 'Trust'" @click="setFlags(d.id, { trusted: !d.trusted })" />
              <FButton v-if="d.verified" variant="ghost" size="sm" icon-only :icon="Pencil" label="Rename" @click="startRename(d)" />
              <FButton v-if="!d.online || d.trusted || d.favorite" variant="ghost" size="sm" icon-only :icon="Trash2" label="Forget" @click="forget(d)" />
            </div>
          </li>
          <li v-if="store.pairing.outgoing?.peer.id === d.id" class="compare" role="status">
            <div>
              <span class="eyebrow">Check that {{ displayName(d) }} shows</span>
              <code class="code tabular">{{ store.pairing.outgoing.code }}</code>
            </div>
            <p>Confirm on {{ displayName(d) }} only if the codes match. Waiting…</p>
            <FButton variant="ghost" size="sm" :icon="X" @click="cancelCompare()">Cancel</FButton>
          </li>
          </template>
        </ul>
      </section>
    </template>

    <div v-if="!devices.length" class="module glass empty">
      <strong>No devices yet</strong>
      <span>Open Ferry or LocalSend on another device on this network.</span>
    </div>

    <PairPanel v-if="platform.capabilities.pairing" />

    <section class="module glass" aria-labelledby="add">
      <div class="module-head">
        <div>
          <h2 id="add">Add by address</h2>
          <p class="sub">For networks that block discovery. Find the address on the other device's Receive page.</p>
        </div>
      </div>
      <form class="add" @submit.prevent="add">
        <input v-model="host" class="input" placeholder="192.168.1.20" aria-label="IP address" inputmode="decimal" />
        <input v-model.number="port" class="input port" aria-label="Port" inputmode="numeric" />
        <FButton type="submit" variant="primary" :icon="Plus" :disabled="!host.trim() || busy">{{ busy ? "Looking…" : "Add" }}</FButton>
      </form>
    </section>
  </div>
</template>

<style scoped>
.module-head .sub {
  margin-top: 2px;
}
.device {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: 10px 0;
}
.text {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  gap: 2px;
}
.text strong {
  font-size: var(--text-md);
  font-weight: 600;
}
.text > span {
  overflow: hidden;
  font-size: var(--text-xs);
  color: var(--text-3);
  white-space: nowrap;
  text-overflow: ellipsis;
}
.rename {
  height: 32px;
  max-width: 280px;
}
.badges {
  display: flex;
  gap: 6px;
  margin-top: 2px;
}
.badge {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 20px;
  padding: 0 7px;
  border-radius: var(--radius-pill);
  background: var(--fill-1);
  font-size: var(--text-2xs);
  font-weight: 560;
  color: var(--text-2);
}
.badge.warn {
  background: var(--warn-soft);
  color: var(--warn);
}
.actions {
  display: flex;
  gap: 2px;
}
.starred :deep(svg) {
  color: var(--accent);
  fill: currentColor;
}
.add {
  display: flex;
  gap: var(--space-2);
  max-width: 520px;
}
.add .input {
  flex: 1;
  min-width: 0;
}
.add .port {
  flex: none;
  width: 90px;
}
@media (max-width: 639px) {
  .device {
    flex-wrap: wrap;
  }
  .actions {
    width: 100%;
    justify-content: flex-end;
  }
}
.compare {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2) var(--space-4);
  margin: 0 0 var(--space-2);
  padding: var(--space-3) var(--space-4);
  border-radius: var(--radius-md);
  background: var(--accent-softer);
}
.compare > div {
  display: flex;
  flex-direction: column;
}
.compare .code {
  font-family: var(--font-mono);
  font-size: var(--text-xl);
  font-weight: 650;
  letter-spacing: 0.12em;
}
.compare p {
  flex: 1;
  min-width: 200px;
  font-size: var(--text-sm);
  color: var(--text-2);
}
</style>
