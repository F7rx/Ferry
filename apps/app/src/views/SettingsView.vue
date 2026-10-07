<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { Activity, Cpu, Info, Lock, MonitorSmartphone, Network, Palette, Settings2, SlidersHorizontal } from "@lucide/vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import FSegmented from "../components/FSegmented.vue";
import FToggle from "../components/FToggle.vue";
import { appearance } from "../lib/appearance";
import { attempt, saveSettings, store, toast } from "../stores/engine";
import { clearHistory, clearHistoryDetail, deleteReceivedFiles, keepsReceivedFiles } from "../stores/history";
import { isNative, platform, type AutoAccept } from "../platform";

const route = useRoute();
const router = useRouter();
const sections = [
  { id: "general", label: "General", icon: Settings2 },
  { id: "devices", label: "Devices", icon: MonitorSmartphone },
  { id: "transfers", label: "Transfers", icon: SlidersHorizontal },
  { id: "privacy", label: "Privacy & Security", icon: Lock },
  { id: "appearance", label: "Appearance", icon: Palette },
  { id: "network", label: "Network", icon: Network },
  { id: "advanced", label: "Advanced", icon: Cpu },
];
const current = computed(() => (route.params.section as string) || "general");
// The browser build talks WebRTC through a signaling server instead of the LAN.
const isWeb = platform.capabilities.kind === "web";
const s = computed(() => store.settings);

// Text fields commit on blur / Enter.
const alias = ref("");
const port = ref(53317);
const group = ref("");
const timeout = ref(300);
const signal = ref("");
const stun = ref("");
watch(
  s,
  (v) => {
    if (!v) return;
    alias.value = v.alias;
    port.value = v.port;
    group.value = v.multicastGroup;
    timeout.value = v.decisionTimeoutSecs;
    signal.value = v.signalingUrl ?? "";
    stun.value = v.stunServers.join(", ");
  },
  { immediate: true },
);
async function commit<K extends keyof NonNullable<typeof s.value>>(key: K, value: NonNullable<typeof s.value>[K]) {
  if (!s.value || s.value[key] === value) return;
  const saved = await saveSettings({ [key]: value } as never);
  if (saved) toast({ level: "success", title: "Saved" }, 1200);
}

// Launch at login (desktop).
const autostart = ref(false);
onMounted(async () => {
  if (!isNative) return;
  try {
    const mod = await import("@tauri-apps/plugin-autostart");
    autostart.value = await mod.isEnabled();
  } catch {
    /* not available */
  }
});
async function setAutostart(on: boolean) {
  const mod = await import("@tauri-apps/plugin-autostart");
  await attempt(() => (on ? mod.enable() : mod.disable()));
  autostart.value = await mod.isEnabled();
}
const setTheme = (v: typeof appearance.theme) => (appearance.theme = v);
const setMotion = (v: typeof appearance.motion) => (appearance.motion = v);
const setTransparency = (v: typeof appearance.transparency) => (appearance.transparency = v);
async function chooseFolder() {
  const dir = await platform.pickSaveFolder();
  if (dir) await saveSettings({ saveDir: dir });
}
async function setEncryption(on: boolean) {
  if (!on && !confirm("Turn off encryption? Transfers would be readable by anyone on this network. Only do this for an old device that can't use encryption.")) return;
  await saveSettings({ encryption: on });
}
async function saveSignal() {
  const url = signal.value.trim();
  if (url && !/^wss?:\/\/[^\s]+$/.test(url)) return toast({ level: "error", title: "Use a ws:// or wss:// address" });
  await commit("signalingUrl", url || null);
}
async function saveStun() {
  const list = stun.value.split(/[\s,]+/).filter(Boolean);
  if (list.some((u) => !/^(stun|turn)s?:/.test(u))) return toast({ level: "error", title: "Use stun: or turn: addresses" });
  if (list.join() !== s.value?.stunServers.join()) await commit("stunServers", list);
}
const acceptOptions: { value: AutoAccept; label: string }[] = [
  { value: "off", label: "Always ask" },
  ...(isWeb ? [] : [{ value: "myDevices" as const, label: "My devices" }]),
  { value: "trusted", label: "Trusted" },
];
</script>

<template>
  <div class="page settings">
    <header class="page-head">
      <div>
        <h1>Settings</h1>
        <p>Sensible defaults. You shouldn't need to change much.</p>
      </div>
    </header>

    <div class="layout">
      <nav class="sections glass" aria-label="Settings sections">
        <button v-for="sec in sections" :key="sec.id" type="button" :class="{ on: current === sec.id }" :aria-current="current === sec.id ? 'page' : undefined" @click="router.replace(`/settings/${sec.id}`)">
          <FIcon :icon="sec.icon" :size="17" />
          <span>{{ sec.label }}</span>
        </button>
        <button type="button" @click="router.push('/diagnostics')">
          <FIcon :icon="Activity" :size="17" />
          <span>Diagnostics</span>
        </button>
      </nav>

      <section v-if="s" class="module glass panel">
        <!-- General -->
        <template v-if="current === 'general'">
          <label class="row">
            <span class="label"><strong>Device name</strong><span>How other devices see this one.</span></span>
            <input v-model="alias" class="input" maxlength="64" @blur="commit('alias', alias.trim() || s.alias)" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <div v-if="isNative" class="row">
            <span class="label"><strong>Open at login</strong><span>Start quietly in the tray so you can always receive.</span></span>
            <FToggle :model-value="autostart" label="Open at login" @update:model-value="setAutostart" />
          </div>
          <div class="row">
            <span class="label"><strong>Receiving</strong><span>Be visible to nearby devices and accept requests.</span></span>
            <FToggle :model-value="s.receiveEnabled" label="Receiving" @update:model-value="(v) => saveSettings({ receiveEnabled: v })" />
          </div>
          <p v-if="isNative" class="note">Closing the window keeps Ferry receiving in the tray. Quit it from the tray icon.</p>
        </template>

        <!-- Devices -->
        <template v-else-if="current === 'devices'">
          <div class="row">
            <span class="label"><strong>Accept without asking</strong><span>Only devices with a verified identity can skip the prompt, never unknown ones.</span></span>
            <FSegmented :model-value="s.autoAccept" :options="acceptOptions" label="Accept without asking" @update:model-value="(v) => saveSettings({ autoAccept: v })" />
          </div>
          <div class="row">
            <span class="label"><strong>Manage devices</strong><span>Rename, trust, or forget devices.</span></span>
            <FButton size="sm" @click="router.push('/devices')">Open Devices</FButton>
          </div>
        </template>

        <!-- Transfers -->
        <template v-else-if="current === 'transfers'">
          <div v-if="isWeb" class="row">
            <span class="label"><strong>Received files</strong><span>Kept in this browser's private storage. Save them from the Inbox.</span></span>
            <FButton size="sm" @click="router.push('/inbox')">Open Inbox</FButton>
          </div>
          <div v-else class="row">
            <span class="label"><strong>Save to</strong><span class="path">{{ s.saveDir ?? "Downloads › Ferry" }}</span></span>
            <FButton size="sm" @click="chooseFolder">Change</FButton>
          </div>
          <label v-if="!isWeb" class="row">
            <span class="label"><strong>Files in parallel</strong><span>More helps with many small files; fewer is gentler on slow disks.</span></span>
            <span class="slider">
              <input type="range" min="1" max="16" :value="s.parallelFiles" @change="saveSettings({ parallelFiles: Number(($event.target as HTMLInputElement).value) })" />
              <output class="tabular">{{ s.parallelFiles }}</output>
            </span>
          </label>
          <div v-if="!isWeb" class="row">
            <span class="label"><strong>Verify files from LocalSend devices</strong><span>Check the SHA-256 they provide. Ferry devices are always verified end to end.</span></span>
            <FToggle :model-value="s.verifyIncomingChecksums" label="Verify incoming checksums" @update:model-value="(v) => saveSettings({ verifyIncomingChecksums: v })" />
          </div>
          <div v-if="!isWeb" class="row">
            <span class="label"><strong>Checksums for LocalSend devices</strong><span>Lets them verify what you send, but reads every file twice.</span></span>
            <FToggle :model-value="s.checksumsForLocalsend" label="Checksums for LocalSend devices" @update:model-value="(v) => saveSettings({ checksumsForLocalsend: v })" />
          </div>
        </template>

        <!-- Privacy & security -->
        <template v-else-if="current === 'privacy'">
          <div v-if="isWeb" class="row">
            <span class="label"><strong>Encryption</strong><span>Always on: WebRTC encrypts every connection, and each device proves its identity key.</span></span>
          </div>
          <div v-else class="row">
            <span class="label"><strong>Encryption</strong><span>HTTPS with mutual TLS. Keep it on.</span></span>
            <FToggle :model-value="s.encryption" label="Encryption" @update:model-value="setEncryption" />
          </div>
          <div class="row">
            <span class="label">
              <strong>Keep a history</strong>
              <span>{{ keepsReceivedFiles ? "File names, sizes and devices, never file contents. Off: only the Inbox's list of files received in this browser is kept." : "File names, sizes and devices, never file contents." }}</span>
            </span>
            <FToggle :model-value="s.historyEnabled" label="Keep a history" @update:model-value="(v) => saveSettings({ historyEnabled: v })" />
          </div>
          <div class="row">
            <span class="label"><strong>Remember message text</strong><span>Off: history shows that a message arrived, not what it said. Needs Keep a history.</span></span>
            <FToggle :model-value="s.keepMessageText" :disabled="!s.historyEnabled" label="Remember message text" @update:model-value="(v) => saveSettings({ keepMessageText: v })" />
          </div>
          <div class="row">
            <span class="label"><strong>Clear history</strong><span>{{ clearHistoryDetail }}</span></span>
            <FButton size="sm" variant="danger" @click="clearHistory">Clear</FButton>
          </div>
          <div v-if="keepsReceivedFiles" class="row">
            <span class="label"><strong>Delete received files</strong><span>Deletes every file kept in this browser. Save the ones you need from the Inbox first.</span></span>
            <FButton size="sm" variant="danger" @click="deleteReceivedFiles">Delete files</FButton>
          </div>
          <p class="note"><FIcon :icon="Info" :size="13" /> Ferry sends no analytics or crash reports, and has no account. Files go directly between your devices.</p>
        </template>

        <!-- Appearance -->
        <template v-else-if="current === 'appearance'">
          <div class="row">
            <span class="label"><strong>Theme</strong></span>
            <FSegmented
              :model-value="appearance.theme"
              :options="[
                { value: 'system', label: 'System' },
                { value: 'light', label: 'Light' },
                { value: 'dark', label: 'Dark' },
              ]"
              label="Theme"
              @update:model-value="setTheme"
            />
          </div>
          <div class="row">
            <span class="label"><strong>Motion</strong><span>Reduced keeps feedback but drops movement.</span></span>
            <FSegmented
              :model-value="appearance.motion"
              :options="[
                { value: 'system', label: 'System' },
                { value: 'reduced', label: 'Reduced' },
                { value: 'full', label: 'Full' },
              ]"
              label="Motion"
              @update:model-value="setMotion"
            />
          </div>
          <div class="row">
            <span class="label"><strong>Transparency</strong><span>Reduced makes surfaces solid (faster on some graphics drivers).</span></span>
            <FSegmented
              :model-value="appearance.transparency"
              :options="[
                { value: 'system', label: 'System' },
                { value: 'reduced', label: 'Reduced' },
                { value: 'full', label: 'Full' },
              ]"
              label="Transparency"
              @update:model-value="setTransparency"
            />
          </div>
        </template>

        <!-- Network -->
        <template v-else-if="current === 'network' && isWeb">
          <label class="row">
            <span class="label"><strong>Signaling server</strong><span>Introduces devices to each other. Files never pass through it. Empty uses this site's server.</span></span>
            <input v-model="signal" class="input" placeholder="wss://signal.example/v1/ws" spellcheck="false" @blur="saveSignal" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <label class="row">
            <span class="label"><strong>STUN servers</strong><span>Help devices on different networks find a direct path. They see your public IP address. Empty: same network only.</span></span>
            <input v-model="stun" class="input" placeholder="stun:stun.example:3478" spellcheck="false" @blur="saveStun" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
        </template>
        <template v-else-if="current === 'network'">
          <label class="row">
            <span class="label"><strong>Port</strong><span>53317 matches LocalSend. Ferry picks the next free port if it's taken.</span></span>
            <input v-model.number="port" class="input narrow" inputmode="numeric" @blur="commit('port', Number(port) || 53317)" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <div class="row">
            <span class="label"><strong>IPv6</strong><span>Also discover devices over IPv6.</span></span>
            <FToggle :model-value="s.ipv6" label="IPv6" @update:model-value="(v) => saveSettings({ ipv6: v })" />
          </div>
          <div class="row">
            <span class="label"><strong>Use VPN and virtual adapters</strong><span>Off keeps this device from announcing itself on VPN, VM or container networks.</span></span>
            <FToggle :model-value="s.includeVirtualInterfaces" label="Use VPN and virtual adapters" @update:model-value="(v) => saveSettings({ includeVirtualInterfaces: v })" />
          </div>
          <div class="row">
            <span class="label"><strong>Scan the network when nothing is found</strong><span>A fallback for networks that block discovery broadcasts.</span></span>
            <FToggle :model-value="s.subnetScan" label="Scan the network" @update:model-value="(v) => saveSettings({ subnetScan: v })" />
          </div>
          <label class="row">
            <span class="label"><strong>Signaling server</strong><span>Optional. Lets browsers and devices on other networks reach this one over WebRTC. It only introduces devices; files never pass through it.</span></span>
            <input v-model="signal" class="input" placeholder="wss://signal.example/v1/ws" spellcheck="false" @blur="saveSignal" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <label v-if="s.signalingUrl" class="row">
            <span class="label"><strong>STUN servers</strong><span>Help devices on different networks find a direct path. They see your public IP address.</span></span>
            <input v-model="stun" class="input" placeholder="stun:stun.example:3478" spellcheck="false" @blur="saveStun" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
        </template>

        <!-- Advanced -->
        <template v-else-if="current === 'advanced'">
          <label v-if="!isWeb" class="row">
            <span class="label"><strong>Multicast group</strong><span>Must match the other devices (LocalSend default 224.0.0.167).</span></span>
            <input v-model="group" class="input narrow" @blur="commit('multicastGroup', group.trim())" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <label class="row">
            <span class="label"><strong>Request timeout</strong><span>Seconds before an unanswered request is declined.</span></span>
            <input v-model.number="timeout" class="input narrow" inputmode="numeric" @blur="commit('decisionTimeoutSecs', Number(timeout) || 300)" @keydown.enter="($event.target as HTMLInputElement).blur()" />
          </label>
          <div class="row about">
            <span class="label">
              <strong>About Ferry</strong>
              <span>Version {{ store.local?.appVersion }} · Apache-2.0</span>
              <span>Speaks the LocalSend protocol and includes software from the LocalSend project (Apache-2.0). Not affiliated with LocalSend.</span>
            </span>
          </div>
        </template>
      </section>
    </div>
  </div>
</template>

<style scoped>
.layout {
  display: grid;
  grid-template-columns: 240px minmax(0, 1fr);
  gap: var(--space-5);
  align-items: start;
}
@media (max-width: 799px) {
  .layout {
    grid-template-columns: minmax(0, 1fr);
  }
  .sections {
    flex-direction: row !important;
    overflow-x: auto;
  }
  .sections button {
    flex: none;
  }
}
.sections {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 8px;
  border-radius: var(--radius-xl);
}
.sections button {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 40px;
  padding: 0 12px;
  border: 0;
  border-radius: var(--radius-sm);
  background: none;
  color: var(--text-2);
  font-size: var(--text-md);
  font-weight: 540;
  text-align: left;
}
@media (hover: hover) {
  .sections button:hover {
    background: var(--fill-hover);
    color: var(--text-1);
  }
}
.sections button.on {
  background: var(--solid-1);
  color: var(--text-1);
  box-shadow:
    var(--shadow-1),
    inset 0 0 0 1px var(--hairline);
}
.sections button.on :deep(svg) {
  color: var(--accent);
}
.panel {
  padding-top: var(--space-2);
  padding-bottom: var(--space-2);
}
.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-5);
  padding: var(--space-4) 0;
}
.row + .row {
  border-top: 1px solid var(--hairline);
}
.label {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.label strong {
  font-size: var(--text-md);
  font-weight: 600;
}
.label span {
  font-size: var(--text-sm);
  color: var(--text-3);
  text-wrap: pretty;
}
.path {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.input {
  width: 260px;
}
.input.narrow {
  width: 140px;
}
.slider {
  display: flex;
  align-items: center;
  gap: 10px;
}
.slider input {
  width: 160px;
  accent-color: var(--accent);
}
.slider output {
  min-width: 2ch;
  font-weight: 600;
}
.value {
  font-size: var(--text-sm);
  color: var(--text-3);
}
.note {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: var(--space-3) 0;
  font-size: var(--text-sm);
  color: var(--text-3);
}
@media (max-width: 639px) {
  .row {
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
  }
  .input {
    width: 100%;
  }
}
</style>
