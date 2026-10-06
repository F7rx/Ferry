<script setup lang="ts">
// Incoming requests as an OS-style handoff: who, what, how big, then
// Decline / Accept / Accept & trust. Partial selection and the save folder
// are one click deeper. Text messages get their own lightweight card.
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { Check, ChevronDown, Copy, ExternalLink, FolderOpen, ShieldAlert, ShieldCheck, X } from "@lucide/vue";
import type { IncomingRequest } from "../platform";
import { platform } from "../platform";
import { respond, store, toast } from "../stores/engine";
import { answerPairing } from "../stores/pairing";
import { fileIcon } from "../lib/icons";
import { firstUrl, formatBytes, plural } from "../lib/format";
import DeviceAvatar from "./DeviceAvatar.vue";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";

const request = computed<IncomingRequest | undefined>(() => store.requests[0]);
const message = computed<IncomingRequest | undefined>(() => store.messages[0]);

const ui = reactive({ details: false, selected: new Set<string>(), saveDir: null as string | null });
watch(
  () => request.value?.id,
  () => {
    ui.details = false;
    ui.saveDir = null;
    ui.selected = new Set(request.value?.files.map((f) => f.id) ?? []);
  },
  { immediate: true },
);

const selectedBytes = computed(() => request.value?.files.filter((f) => ui.selected.has(f.id)).reduce((n, f) => n + f.size, 0) ?? 0);
const allSelected = computed(() => !!request.value && ui.selected.size === request.value.files.length);
const kinds = computed(() => {
  const r = request.value;
  if (!r) return "";
  const images = r.files.filter((f) => f.mime.startsWith("image/")).length;
  if (images === r.files.length) return plural(images, "photo");
  return plural(r.files.length, "item");
});

// Expiry countdown (auto-decline happens in the engine).
const now = ref(Date.now());
let timer: number | undefined;
onMounted(() => (timer = window.setInterval(() => (now.value = Date.now()), 1000)));
onBeforeUnmount(() => clearInterval(timer));
const remaining = computed(() => (request.value ? Math.max(0, request.value.expiresAtMs - now.value) : 0));
const remainingFraction = computed(() => (request.value ? remaining.value / Math.max(1, request.value.expiresAtMs - request.value.receivedAtMs) : 0));

function toggle(id: string) {
  if (ui.selected.has(id)) ui.selected.delete(id);
  else ui.selected.add(id);
}
async function chooseFolder() {
  const dir = await platform.pickSaveFolder();
  if (dir) ui.saveDir = dir;
}
function accept(trust = false) {
  const r = request.value;
  if (!r) return;
  void respond(r, { accept: allSelected.value ? null : [...ui.selected], trust, saveDir: ui.saveDir });
}
function decline() {
  const r = request.value;
  if (r) void respond(r, { decline: true });
}
function onKey(e: KeyboardEvent) {
  if (!request.value) return;
  // Keys pressed on another control (Decline, Change, a link…) belong to that control.
  const target = e.target as HTMLElement | null;
  if (target?.closest("input, textarea, select, button, a, [role=button], [contenteditable]")) return;
  if (e.key === "Escape") decline();
  if (e.key === "Enter" && ui.selected.size) accept();
}
onMounted(() => window.addEventListener("keydown", onKey));
onBeforeUnmount(() => window.removeEventListener("keydown", onKey));

// Pairing by code comparison. No keyboard shortcut for "They match": it
// grants lasting trust, so it takes a deliberate click.
const pairRequest = computed(() => store.pairing.requests[0]);
const pairFraction = computed(() => (pairRequest.value ? Math.max(0, pairRequest.value.expiresAtMs - now.value) / 120_000 : 0));
function answerPair(match: boolean) {
  if (pairRequest.value) void answerPairing(pairRequest.value, match);
}

function dismissMessage() {
  store.messages.shift();
}
async function copyMessage() {
  if (!message.value?.text) return;
  await platform.copyText(message.value.text);
  toast({ level: "success", title: "Copied" }, 1800);
  dismissMessage();
}
const messageLink = computed(() => (message.value?.text ? firstUrl(message.value.text) : null));
function openLink() {
  if (messageLink.value) window.open(messageLink.value, "_blank", "noopener,noreferrer");
  dismissMessage();
}
</script>

<template>
  <div class="handoff-layer" aria-live="polite">
    <Transition name="handoff">
      <section v-if="pairRequest" :key="pairRequest.id" class="handoff pairing glass-2" role="alertdialog" aria-modal="false" :aria-label="`${pairRequest.peer.alias} wants to pair`">
        <svg class="expiry" viewBox="0 0 100 2" preserveAspectRatio="none" aria-hidden="true">
          <line x1="0" y1="1" :x2="pairFraction * 100" y2="1" />
        </svg>
        <header>
          <DeviceAvatar :kind="pairRequest.peer.deviceKind" :model="pairRequest.peer.deviceModel" :size="48" />
          <div class="who">
            <h2>{{ pairRequest.peer.alias }}</h2>
            <p>wants to add this device to its devices</p>
          </div>
        </header>
        <div class="pair-code">
          <span class="eyebrow">Same code on both screens?</span>
          <code class="tabular">{{ pairRequest.code }}</code>
        </div>
        <p class="pair-note">Paired devices send files to each other without asking. If the codes differ, someone else may be on this network.</p>
        <footer>
          <FButton variant="ghost" @click="answerPair(false)">Don't match</FButton>
          <span class="spacer" />
          <FButton variant="primary" :icon="Check" @click="answerPair(true)">They match</FButton>
        </footer>
      </section>
    </Transition>

    <Transition name="handoff" mode="out-in">
      <section v-if="request" :key="request.id" class="handoff glass-2" role="dialog" aria-modal="false" :aria-label="`${request.peer.alias} wants to send ${kinds}`">
        <svg class="expiry" viewBox="0 0 100 2" preserveAspectRatio="none" aria-hidden="true">
          <line x1="0" y1="1" :x2="remainingFraction * 100" y2="1" />
        </svg>
        <header>
          <DeviceAvatar :kind="request.peer.deviceKind" :model="request.peer.deviceModel" :size="48" />
          <div class="who">
            <h2>{{ request.peer.alias }}</h2>
            <p>
              wants to send {{ kinds }} · <span class="tabular">{{ formatBytes(request.totalBytes) }}</span>
            </p>
            <p class="identity" :class="{ ok: request.peer.verified && request.trusted, warn: !request.peer.verified }">
              <FIcon :icon="request.peer.verified ? ShieldCheck : ShieldAlert" :size="13" />
              <span v-if="!request.peer.verified">Identity not verified. Not encrypted.</span>
              <span v-else-if="request.trusted">Trusted device</span>
              <span v-else>Verified identity · not trusted yet</span>
            </p>
          </div>
        </header>

        <ul class="preview" :class="{ open: ui.details }">
          <li v-for="f in ui.details ? request.files : request.files.slice(0, 3)" :key="f.id">
            <label :class="{ off: !ui.selected.has(f.id) }">
              <input v-if="ui.details" type="checkbox" :checked="ui.selected.has(f.id)" @change="toggle(f.id)" />
              <span class="ficon"><FIcon :icon="fileIcon(f.name, f.mime)" :size="15" /></span>
              <span class="fname">{{ f.name }}</span>
              <span class="fsize tabular">{{ formatBytes(f.size) }}</span>
            </label>
          </li>
          <li v-if="!ui.details && request.files.length > 3" class="rest">and {{ request.files.length - 3 }} more</li>
        </ul>

        <button type="button" class="details" :aria-expanded="ui.details" @click="ui.details = !ui.details">
          {{ ui.details ? "Fewer options" : "Choose files & location" }}
          <FIcon :icon="ChevronDown" :size="14" :class="{ flip: ui.details }" />
        </button>
        <div v-if="ui.details" class="where">
          <FIcon :icon="FolderOpen" :size="15" />
          <span class="path" :title="ui.saveDir ?? request.defaultSaveDir">{{ ui.saveDir ?? request.defaultSaveDir }}</span>
          <FButton size="sm" variant="ghost" @click="chooseFolder">Change</FButton>
        </div>

        <footer>
          <FButton variant="ghost" @click="decline">Decline</FButton>
          <span class="spacer" />
          <FButton v-if="request.peer.verified && !request.trusted" :disabled="!ui.selected.size" @click="accept(true)">Accept & trust</FButton>
          <FButton variant="primary" :icon="Check" :disabled="!ui.selected.size" @click="accept()">
            {{ allSelected ? "Accept" : `Accept ${ui.selected.size}` }}
          </FButton>
        </footer>
        <p v-if="ui.details && !allSelected" class="partial tabular">{{ formatBytes(selectedBytes) }} selected</p>
      </section>
    </Transition>

    <Transition name="handoff">
      <section v-if="message && !request" :key="message.id" class="handoff message glass-2" role="dialog" aria-modal="false" :aria-label="`Message from ${message.peer.alias}`">
        <header>
          <DeviceAvatar :kind="message.peer.deviceKind" :model="message.peer.deviceModel" :size="40" />
          <div class="who">
            <h2>{{ message.peer.alias }}</h2>
            <p class="identity" :class="{ warn: !message.peer.verified }">
              <FIcon :icon="message.peer.verified ? ShieldCheck : ShieldAlert" :size="13" />
              {{ message.peer.verified ? "Sent you a message" : "Unverified sender" }}
            </p>
          </div>
          <FButton variant="ghost" size="sm" icon-only :icon="X" label="Dismiss" @click="dismissMessage" />
        </header>
        <p class="body">{{ message.text }}</p>
        <footer>
          <span class="spacer" />
          <FButton v-if="messageLink" :icon="ExternalLink" @click="openLink">Open link</FButton>
          <FButton variant="primary" :icon="Copy" @click="copyMessage">Copy</FButton>
        </footer>
      </section>
    </Transition>
  </div>
</template>

<style scoped>
.handoff-layer {
  position: fixed;
  top: 20px;
  right: 20px;
  z-index: 50;
  display: flex;
  flex-direction: column;
  gap: 12px;
  width: min(392px, calc(100vw - 32px));
  pointer-events: none;
}
.handoff {
  position: relative;
  overflow: hidden;
  pointer-events: auto;
  padding: 18px 18px 16px;
  border-radius: var(--radius-xl);
}
.pair-code {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  margin: 14px 0 8px;
  padding: 14px;
  border-radius: var(--radius-lg);
  background: var(--fill-1);
}
.pair-code code {
  font-family: var(--font-mono);
  font-size: 30px;
  font-weight: 650;
  letter-spacing: 0.14em;
  color: var(--text-1);
}
.pair-note {
  margin-bottom: 14px;
  font-size: var(--text-sm);
  color: var(--text-3);
  text-wrap: pretty;
}
.expiry {
  position: absolute;
  top: 0;
  left: 0;
  width: 100%;
  height: 2px;
}
.expiry line {
  stroke: var(--accent);
  stroke-width: 2;
  opacity: 0.5;
  transition: x2 1s linear;
}
header {
  display: flex;
  align-items: flex-start;
  gap: 12px;
}
.who {
  flex: 1;
  min-width: 0;
}
h2 {
  overflow: hidden;
  font-size: var(--text-lg);
  font-weight: 640;
  letter-spacing: var(--track-title);
  line-height: 1.25;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.who p {
  font-size: var(--text-md);
  color: var(--text-2);
}
.identity {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin-top: 2px;
  font-size: var(--text-xs) !important;
  color: var(--text-3) !important;
}
.identity.ok {
  color: var(--ok) !important;
}
.identity.warn {
  color: var(--warn) !important;
}
.preview {
  list-style: none;
  margin: 14px 0 0;
  padding: 4px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
}
.preview.open {
  max-height: 260px;
  overflow: auto;
}
.preview label {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 36px;
  padding: 0 8px;
  border-radius: var(--radius-sm);
  font-size: var(--text-sm);
}
.preview.open label {
  cursor: pointer;
}
@media (hover: hover) {
  .preview.open label:hover {
    background: var(--fill-hover);
  }
}
.preview label.off {
  opacity: 0.5;
}
.preview input {
  accent-color: var(--accent);
  width: 16px;
  height: 16px;
}
.ficon {
  color: var(--accent-strong);
}
:root[data-theme="dark"] .ficon {
  color: var(--accent-text);
}
.fname {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.fsize {
  color: var(--text-3);
  font-size: var(--text-xs);
}
.rest {
  padding: 4px 8px 6px 38px;
  font-size: var(--text-xs);
  color: var(--text-3);
}
.details {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin-top: 10px;
  padding: 4px 2px;
  border: 0;
  background: none;
  font-size: var(--text-sm);
  font-weight: 560;
  color: var(--accent-text);
}
.flip {
  transform: rotate(180deg);
}
.where {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 6px;
  color: var(--text-3);
  font-size: var(--text-sm);
}
.path {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  direction: rtl;
  text-align: left;
}
footer {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 16px;
}
.spacer {
  flex: 1;
}
.partial {
  margin-top: 6px;
  text-align: right;
  font-size: var(--text-xs);
  color: var(--text-3);
}
.message .body {
  margin-top: 12px;
  padding: 12px 14px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
  font-size: var(--text-base);
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  max-height: 220px;
  overflow: auto;
}

/* Enters and leaves along the same path, anchored top-right. */
.handoff-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-soft-dur) var(--spring-soft),
    filter var(--dur-panel) var(--ease-out);
}
.handoff-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-in),
    transform var(--dur-small) var(--ease-in);
}
.handoff-enter-from,
.handoff-leave-to {
  opacity: 0;
  transform: translateY(calc(-14px * var(--motion-scale))) scale(calc(1 - 0.03 * var(--motion-scale)));
}
.handoff-enter-from {
  filter: blur(calc(6px * var(--motion-scale)));
}

@media (max-width: 639px) {
  .handoff-layer {
    top: auto;
    bottom: calc(76px + env(safe-area-inset-bottom));
    right: 12px;
    left: 12px;
    width: auto;
  }
  .handoff-enter-from,
  .handoff-leave-to {
    transform: translateY(calc(18px * var(--motion-scale)));
  }
}
</style>
