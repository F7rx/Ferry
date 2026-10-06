<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { Copy, FolderOpen, Globe, KeyRound } from "@lucide/vue";
import BrowserLinkCard from "../components/BrowserLinkCard.vue";
import DeviceAvatar from "../components/DeviceAvatar.vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import FSegmented from "../components/FSegmented.vue";
import FToggle from "../components/FToggle.vue";
import TransferCard from "../components/TransferCard.vue";
import { saveSettings, store, toast, transfers } from "../stores/engine";
import { platform, type AutoAccept } from "../platform";
import { randomPin, receiveByLink, uploadLinks } from "../stores/links";

const s = computed(() => store.settings);
const isWeb = platform.capabilities.kind === "web";
const incoming = computed(() => transfers.value.filter((t) => t.direction === "receive").slice(0, 6));
const alias = ref(store.settings?.alias ?? "");
watch(() => store.settings?.alias, (a) => (alias.value = a ?? ""));
const pinDraft = ref(store.settings?.pin ?? "");

async function saveAlias() {
  const v = alias.value.trim();
  if (v && v !== store.settings?.alias) {
    const saved = await saveSettings({ alias: v });
    if (saved) toast({ level: "success", title: `Now visible as ${saved.alias}` }, 2500);
  }
}
async function togglePin(on: boolean) {
  if (!on) await saveSettings({ pin: null });
  else {
    const pin = pinDraft.value.trim() || String(Math.floor(100000 + Math.random() * 900000));
    pinDraft.value = pin;
    await saveSettings({ pin });
  }
}
async function savePin() {
  if (pinDraft.value.trim()) await saveSettings({ pin: pinDraft.value.trim() });
}
async function chooseFolder() {
  const dir = await platform.pickSaveFolder();
  if (dir) await saveSettings({ saveDir: dir });
}
async function copy(text: string) {
  await platform.copyText(text);
  toast({ level: "success", title: "Copied" }, 1500);
}
const linkPin = ref(false);
const creating = ref(false);
async function createUploadLink() {
  creating.value = true;
  await receiveByLink(linkPin.value ? randomPin() : null);
  creating.value = false;
}

const acceptOptions: { value: AutoAccept; label: string }[] = [
  { value: "off", label: "Always ask" },
  { value: "myDevices", label: "My devices" },
  { value: "trusted", label: "Trusted" },
];
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Receive</h1>
        <p>Who can find this device, and what happens when they send something.</p>
      </div>
    </header>

    <div class="cols">
      <section class="module glass identity" aria-labelledby="me">
        <div class="me">
          <DeviceAvatar v-if="store.local" :kind="store.local.deviceKind" :model="store.local.deviceModel" :size="64" tone="accent" />
          <div class="me-text">
            <label for="alias" class="eyebrow">Device name</label>
            <input id="alias" v-model="alias" class="name-input" maxlength="64" @blur="saveAlias" @keydown.enter="($event.target as HTMLInputElement).blur()" />
            <p id="me" class="where">
              <template v-if="isWeb">{{ store.server.running ? "Online" : "Offline" }} · Encrypted (WebRTC)</template>
              <template v-else>
                <template v-if="store.local?.addresses.length">{{ store.local.addresses[0] }}:{{ store.local.port }}</template>
                <template v-else>No network</template>
                · {{ store.local?.protocol === "https" ? "Encrypted" : "Not encrypted" }}
              </template>
            </p>
          </div>
        </div>
        <div class="fingerprint">
          <span class="eyebrow">Identity code</span>
          <code class="tabular">{{ store.local?.shortId }}</code>
          <FButton variant="ghost" size="sm" icon-only :icon="Copy" label="Copy identity code" @click="copy(store.local?.fingerprint ?? '')" />
        </div>
        <p class="note">Others see this code next to your name. Compare it in person to be sure you're sending to the right device.</p>
      </section>

      <section class="module glass" aria-labelledby="rules">
        <div class="module-head">
          <h2 id="rules">Receiving</h2>
          <FToggle :model-value="s?.receiveEnabled !== false" label="Receiving" @update:model-value="(v) => saveSettings({ receiveEnabled: v })" />
        </div>
        <div class="setting">
          <div>
            <strong>Accept without asking</strong>
            <span>Only devices with a verified identity can skip the prompt.</span>
          </div>
          <FSegmented :model-value="s?.autoAccept ?? 'myDevices'" :options="acceptOptions" label="Accept without asking" @update:model-value="(v) => saveSettings({ autoAccept: v })" />
        </div>
        <div v-if="!isWeb" class="setting">
          <div>
            <strong><FIcon :icon="KeyRound" :size="14" /> Require a PIN</strong>
            <span>Untrusted senders must enter it first.</span>
          </div>
          <FToggle :model-value="!!s?.pin" label="Require a PIN" @update:model-value="togglePin" />
        </div>
        <div v-if="s?.pin && !isWeb" class="pin-row">
          <input v-model="pinDraft" class="input pin" maxlength="32" aria-label="PIN" @blur="savePin" />
          <span class="note">Share it in person, never in the same channel as the file.</span>
        </div>
        <div class="setting">
          <div>
            <strong><FIcon :icon="FolderOpen" :size="14" /> Save to</strong>
            <span class="path">{{ isWeb ? "This browser. Save files from the Inbox." : (s?.saveDir ?? "Downloads › Ferry") }}</span>
          </div>
          <FButton v-if="!isWeb" size="sm" @click="chooseFolder">Change</FButton>
        </div>
      </section>
    </div>

    <section v-if="platform.capabilities.browserLinks" class="module glass" aria-labelledby="from-browser">
      <div class="module-head">
        <h2 id="from-browser"><FIcon :icon="Globe" :size="16" /> Receive from a browser</h2>
      </div>
      <p class="lede">
        For a phone or laptop without Ferry: open a link in any browser on this network and pick files. You still accept each
        upload here.
      </p>
      <div v-if="uploadLinks.length" class="links">
        <BrowserLinkCard v-for="l in uploadLinks" :key="l.id" :link="l" />
      </div>
      <div v-else class="create">
        <label class="check">
          <input v-model="linkPin" type="checkbox" />
          Ask for a PIN
        </label>
        <FButton variant="primary" :icon="Globe" :disabled="creating || s?.receiveEnabled === false" @click="createUploadLink">Create upload link</FButton>
      </div>
      <p v-if="s?.receiveEnabled === false" class="note">Turn on receiving to create an upload link.</p>
    </section>

    <section class="module glass" aria-labelledby="incoming">
      <div class="module-head">
        <h2 id="incoming">Incoming</h2>
      </div>
      <div v-if="incoming.length" class="stack">
        <TransferCard v-for="t in incoming" :key="t.id" :transfer="t" />
      </div>
      <p v-else class="quiet">Nothing received yet in this session. Requests appear in the top-right corner.</p>
    </section>
  </div>
</template>

<style scoped>
.cols {
  display: grid;
  grid-template-columns: minmax(0, 5fr) minmax(0, 7fr);
  gap: var(--space-5);
  align-items: start;
}
@media (max-width: 999px) {
  .cols {
    grid-template-columns: minmax(0, 1fr);
  }
}
.me {
  display: flex;
  align-items: center;
  gap: var(--space-4);
}
.me-text {
  flex: 1;
  min-width: 0;
}
.name-input {
  width: 100%;
  margin-top: 2px;
  padding: 2px 0;
  border: 0;
  border-bottom: 1.5px solid transparent;
  background: none;
  font-size: var(--text-xl);
  font-weight: 680;
  letter-spacing: var(--track-title);
}
@media (hover: hover) {
  .name-input:hover {
    border-bottom-color: var(--hairline-strong);
  }
}
.name-input:focus {
  outline: none;
  border-bottom-color: var(--accent);
}
.where {
  font-size: var(--text-sm);
  color: var(--text-3);
}
.fingerprint {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-top: var(--space-5);
  padding: 10px 8px 10px 14px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
}
.fingerprint code {
  flex: 1;
  font-family: var(--font-mono);
  font-size: var(--text-md);
  letter-spacing: 0.06em;
}
.note {
  margin-top: var(--space-2);
  font-size: var(--text-xs);
  color: var(--text-3);
}
.setting {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: var(--space-3) 0;
  border-top: 1px solid var(--hairline);
}
.setting:first-of-type {
  border-top: 0;
}
.setting > div:first-child {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.setting strong {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--text-md);
  font-weight: 600;
}
.setting span {
  font-size: var(--text-sm);
  color: var(--text-3);
}
.path {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.pin-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding-bottom: var(--space-3);
}
.pin {
  width: 140px;
  font-family: var(--font-mono);
  letter-spacing: 0.2em;
}
.module-head h2 {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}
.lede {
  max-width: 64ch;
  font-size: var(--text-sm);
  color: var(--text-2);
}
.links {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(min(100%, 380px), 1fr));
  gap: var(--space-3);
  margin-top: var(--space-3);
}
.create {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: flex-end;
  gap: var(--space-4);
  margin-top: var(--space-3);
}
.check {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  min-height: 44px;
  font-size: var(--text-sm);
  color: var(--text-2);
}
.check input {
  width: 18px;
  height: 18px;
  accent-color: var(--accent);
}
.stack {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.quiet {
  color: var(--text-3);
  font-size: var(--text-sm);
}
@media (max-width: 639px) {
  .setting {
    flex-direction: column;
    align-items: flex-start;
  }
}
</style>
