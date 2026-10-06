<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import { useRouter } from "vue-router";
import { ArrowRight, ClipboardPaste, Lock, Radio, ShieldCheck } from "@lucide/vue";
import BrowserLinkCard from "../components/BrowserLinkCard.vue";
import DropStage from "../components/DropStage.vue";
import RoomPanel from "../components/RoomPanel.vue";
import TransferCard from "../components/TransferCard.vue";
import DeviceAvatar from "../components/DeviceAvatar.vue";
import FIcon from "../components/FIcon.vue";
import FToggle from "../components/FToggle.vue";
import { activeTransfers, displayName, onlineDevices, quickDevices, saveSettings, store, transfers } from "../stores/engine";
import { sendNow } from "../stores/compose";
import { platform } from "../platform";
import { formatBytes, plural, relativeTime } from "../lib/format";
import { fileIcon } from "../lib/icons";
import { links } from "../stores/links";
import { motionReduced } from "../lib/appearance";

const router = useRouter();
const receiving = computed(() => store.settings?.receiveEnabled !== false);
const recent = computed(() => transfers.value.filter((t) => !activeTransfers.value.includes(t)).slice(0, 3));
const shownTransfers = computed(() => [...activeTransfers.value, ...recent.value].slice(0, 4));
const lastReceived = computed(() => store.history.filter((h) => h.direction === "receive").slice(0, 3));
const autoAcceptLabel = computed(() => ({ off: "Always ask", myDevices: "My devices", trusted: "Trusted devices" })[store.settings?.autoAccept ?? "myDevices"]);
const clipboardTargets = computed(() => quickDevices.value.filter((d) => d.online).slice(0, 4));

// A new link may land below the fold on narrow layouts; bring it into view.
const linksEl = ref<HTMLElement | null>(null);
watch(
  () => links.value.length,
  async (n, prev) => {
    if (n <= (prev ?? 0)) return;
    await nextTick();
    linksEl.value?.scrollIntoView({ block: "nearest", behavior: motionReduced() ? "auto" : "smooth" });
  },
);

async function sendClipboard(id: string) {
  const item = await platform.readClipboard();
  if (item) await sendNow(id, [item]);
}
</script>

<template>
  <div class="page home">
    <div class="layout">
      <section class="hero glass" aria-labelledby="hero-title">
        <header class="hero-head">
          <div>
            <p class="eyebrow">
              <FIcon :icon="Radio" :size="13" />
              {{ platform.capabilities.lanDiscovery ? "Nearby" : "Devices" }} · {{ onlineDevices.length ? plural(onlineDevices.length, "device") : "searching" }}
            </p>
            <h1 id="hero-title" class="display">Drop anything.</h1>
            <p class="sub">Files, folders, text and links go straight to your devices. Encrypted, with no cloud in between.</p>
          </div>
          <span class="secure"><FIcon :icon="Lock" :size="13" /> End-to-end encrypted</span>
        </header>
        <DropStage />
      </section>

      <aside class="side">
        <section class="module glass receiving" :class="{ off: !receiving }" aria-labelledby="receiving-title">
          <div class="module-head">
            <h2 id="receiving-title">Receiving</h2>
            <FToggle :model-value="receiving" label="Receiving" @update:model-value="(v) => saveSettings({ receiveEnabled: v })" />
          </div>
          <p class="status">
            <i class="pulse" aria-hidden="true" />
            <template v-if="!store.server.running">{{ store.server.error ?? "Not running" }}</template>
            <template v-else-if="receiving">Visible as <strong>{{ store.local?.alias }}</strong></template>
            <template v-else>Hidden. Others can't send to you.</template>
          </p>
          <dl class="facts">
            <div>
              <dt>Accept automatically</dt>
              <dd>{{ autoAcceptLabel }}</dd>
            </div>
            <div v-if="platform.capabilities.kind === 'web'">
              <dt>Connection</dt>
              <dd>{{ store.server.running ? "Online" : "Offline" }}</dd>
            </div>
            <div v-else>
              <dt>PIN</dt>
              <dd>{{ store.settings?.pin ? "On" : "Off" }}</dd>
            </div>
          </dl>
          <button type="button" class="more-link" @click="router.push('/receive')">Receive options <FIcon :icon="ArrowRight" :size="14" /></button>
        </section>

        <Transition name="stack">
          <section v-if="links.length" ref="linksEl" class="module glass" aria-labelledby="links-title">
            <div class="module-head">
              <h2 id="links-title">Browser links</h2>
              <span class="sub">{{ links.length > 1 ? `${links.length} open` : "" }}</span>
            </div>
            <TransitionGroup tag="div" name="stack" class="stack">
              <BrowserLinkCard v-for="l in links" :key="l.id" :link="l" />
            </TransitionGroup>
          </section>
        </Transition>

        <RoomPanel v-if="platform.capabilities.remoteLinks" />

        <section class="module glass" aria-labelledby="transfers-title">
          <div class="module-head">
            <h2 id="transfers-title">Transfers</h2>
            <span class="sub">{{ activeTransfers.length ? `${activeTransfers.length} active` : "" }}</span>
          </div>
          <TransitionGroup v-if="shownTransfers.length" tag="div" name="stack" class="stack">
            <TransferCard v-for="t in shownTransfers" :key="t.id" :transfer="t" />
          </TransitionGroup>
          <div v-else-if="lastReceived.length" class="recent">
            <p class="eyebrow">Recently received</p>
            <ul class="row-list">
              <li v-for="h in lastReceived" :key="h.id" class="recent-row">
                <span class="ricon"><FIcon :icon="fileIcon(h.name, h.mime)" :size="16" /></span>
                <span class="rname">{{ h.name }}</span>
                <span class="rmeta tabular">{{ formatBytes(h.size) }} · {{ relativeTime(h.timestampMs) }}</span>
              </li>
            </ul>
          </div>
          <p v-else class="quiet">Nothing in flight. Drop something on a device to start.</p>
          <button v-if="transfers.length > 4 || lastReceived.length" type="button" class="more-link" @click="router.push('/transfers')">
            All transfers <FIcon :icon="ArrowRight" :size="14" />
          </button>
        </section>

        <section v-if="quickDevices.length" class="module glass" aria-labelledby="quick-title">
          <div class="module-head">
            <h2 id="quick-title">Quick devices</h2>
            <span class="sub">Drag files onto one to send</span>
          </div>
          <ul class="quick">
            <li v-for="d in quickDevices.slice(0, 5)" :key="d.id">
              <button type="button" class="quick-item" :data-device-id="d.id" :aria-label="`Send files to ${displayName(d)}`" @click="router.push('/send?to=' + encodeURIComponent(d.id))">
                <DeviceAvatar :kind="d.deviceKind" :model="d.deviceModel" :size="38" :online="d.online" />
                <span class="qname">{{ displayName(d) }}</span>
                <span class="qstate" :class="{ on: d.online }">{{ d.online ? "Online" : "Offline" }}</span>
                <FIcon v-if="d.mine" :icon="ShieldCheck" :size="14" class="mine" />
              </button>
            </li>
          </ul>
        </section>

        <section v-if="platform.capabilities.clipboardRead && clipboardTargets.length" class="module glass" aria-labelledby="clip-title">
          <div class="module-head">
            <h2 id="clip-title">Clipboard</h2>
            <FIcon :icon="ClipboardPaste" :size="16" class="muted" />
          </div>
          <div class="clip">
            <button v-for="d in clipboardTargets" :key="d.id" type="button" class="clip-btn" @click="sendClipboard(d.id)">
              Send clipboard to <strong>{{ displayName(d) }}</strong>
            </button>
          </div>
          <p class="quiet small">Ferry reads the clipboard only when you press a button.</p>
        </section>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.layout {
  display: grid;
  grid-template-columns: minmax(0, 8fr) minmax(300px, 4fr);
  gap: var(--space-5);
  align-items: start;
}
@media (max-width: 1099px) {
  .layout {
    grid-template-columns: minmax(0, 1fr);
  }
}
.hero {
  padding: var(--space-6) var(--space-6) var(--space-5);
  border-radius: var(--radius-xl);
}
.hero-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-4);
  margin-bottom: var(--space-2);
}
.eyebrow {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.display {
  margin-top: 6px;
  font-size: var(--text-display);
  font-weight: 700;
  letter-spacing: var(--track-display);
  line-height: 1.02;
  background: linear-gradient(180deg, var(--text-1) 30%, color-mix(in srgb, var(--text-1) 72%, var(--accent-indigo)));
  -webkit-background-clip: text;
  background-clip: text;
  color: transparent;
}
.sub {
  max-width: 46ch;
  margin-top: 10px;
  font-size: var(--text-base);
  color: var(--text-2);
  text-wrap: pretty;
}
.secure {
  display: inline-flex;
  flex: none;
  align-items: center;
  gap: 6px;
  height: 28px;
  padding: 0 12px;
  border-radius: var(--radius-pill);
  background: var(--ok-soft);
  color: var(--ok);
  font-size: var(--text-xs);
  font-weight: 600;
}
.side {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.receiving .status {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--text-md);
  color: var(--text-2);
}
.receiving .status strong {
  color: var(--text-1);
  font-weight: 600;
}
.pulse {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--ok);
  box-shadow: 0 0 0 4px var(--ok-soft);
  animation: breathe 3.2s var(--ease-in-out) infinite;
}
.receiving.off .pulse {
  background: var(--text-3);
  box-shadow: none;
  animation: none;
}
:root[data-motion="reduced"] .pulse {
  animation: none;
}
@keyframes breathe {
  50% {
    box-shadow: 0 0 0 7px var(--ok-soft);
  }
}
.facts {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: var(--space-2);
  margin: var(--space-4) 0 0;
}
.facts div {
  padding: 10px 12px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
}
.facts dt {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.facts dd {
  margin: 2px 0 0;
  font-size: var(--text-md);
  font-weight: 600;
}
.more-link {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin-top: var(--space-3);
  padding: 4px 0;
  border: 0;
  background: none;
  color: var(--accent-text);
  font-size: var(--text-sm);
  font-weight: 600;
}
.stack {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.stack-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-soft-dur) var(--spring-soft);
}
.stack-leave-active {
  transition: opacity var(--dur-small) var(--ease-in);
}
.stack-enter-from {
  opacity: 0;
  transform: translateY(calc(-8px * var(--motion-scale))) scale(calc(1 - 0.02 * var(--motion-scale)));
}
.stack-leave-to {
  opacity: 0;
}
.stack-move {
  transition: transform var(--dur-panel) var(--ease-out);
}
.quiet {
  font-size: var(--text-sm);
  color: var(--text-3);
}
.quiet.small {
  margin-top: var(--space-2);
  font-size: var(--text-xs);
}
.recent .eyebrow {
  margin-bottom: 6px;
}
.recent-row {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 44px;
}
.ricon {
  display: grid;
  place-items: center;
  width: 30px;
  height: 30px;
  border-radius: var(--radius-xs);
  background: var(--accent-softer);
  color: var(--accent-strong);
}
.rname {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  font-size: var(--text-sm);
  font-weight: 560;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.rmeta {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.quick {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.quick-item {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  min-height: 52px;
  padding: 6px 8px;
  border: 1px solid transparent;
  border-radius: var(--radius-md);
  background: none;
  text-align: left;
  transition:
    background-color var(--dur-control) var(--ease-out),
    border-color var(--dur-control) var(--ease-out);
}
@media (hover: hover) {
  .quick-item:hover {
    background: var(--fill-hover);
  }
}
.qname {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  font-size: var(--text-md);
  font-weight: 560;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.qstate {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.qstate.on {
  color: var(--ok);
}
.mine {
  color: var(--ok);
}
.muted {
  color: var(--text-3);
}
.clip {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.clip-btn {
  min-height: 40px;
  padding: 8px 12px;
  border: 1px solid var(--hairline);
  border-radius: var(--radius-md);
  background: var(--fill-1);
  text-align: left;
  font-size: var(--text-sm);
  color: var(--text-2);
  transition: background-color var(--dur-control) var(--ease-out);
}
@media (hover: hover) {
  .clip-btn:hover {
    background: var(--fill-2);
  }
}
.clip-btn strong {
  color: var(--text-1);
  font-weight: 600;
}
@media (max-width: 639px) {
  .hero {
    padding: var(--space-5) var(--space-4);
  }
  .secure {
    display: none;
  }
}
</style>
