<script setup lang="ts">
// "Add one of my devices": show a single-use QR code / link, or use one from
// the other device. Code comparison with a nearby device lives on its row.
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { renderSVG } from "uqr";
import { Copy, Link2, QrCode, X } from "@lucide/vue";
import { platform } from "../platform";
import { store, toast } from "../stores/engine";
import { hidePairingCode, pairWithLink, showPairingCode } from "../stores/pairing";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";

const offer = computed(() => store.pairing.offer);
const qr = computed(() => (offer.value ? renderSVG(offer.value.uri, { border: 1 }) : ""));

const now = ref(Date.now());
let timer: number | undefined;
onMounted(() => (timer = window.setInterval(() => (now.value = Date.now()), 1000)));
onBeforeUnmount(() => {
  clearInterval(timer);
  // A code on screen is consent; don't leave one valid after leaving the page.
  if (store.pairing.offer) void hidePairingCode();
});
const remaining = computed(() => {
  const s = Math.max(0, Math.round(((offer.value?.expiresAtMs ?? 0) - now.value) / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
});

const showing = ref(false);
async function show() {
  showing.value = true;
  await showPairingCode();
  showing.value = false;
}
async function copy() {
  if (!offer.value) return;
  await platform.copyText(offer.value.uri);
  toast({ level: "success", title: "Pairing link copied", body: "Paste it into Ferry on your other device. It works once." }, 3200);
}

const link = ref("");
const pairing = ref(false);
async function usePairLink() {
  pairing.value = true;
  const device = await pairWithLink(link.value);
  pairing.value = false;
  if (device) link.value = "";
}
</script>

<template>
  <section class="module glass pair" aria-labelledby="pair-title">
    <div class="module-head">
      <div>
        <h2 id="pair-title">Add one of my devices</h2>
        <p class="sub">Paired devices trust each other: files arrive without asking. Pairing needs a code only you can see.</p>
      </div>
    </div>

    <Transition name="swap" mode="out-in">
      <div v-if="offer" key="offer" class="offer">
        <!-- SVG generated locally by uqr from the pairing link -->
        <div class="qr" role="img" aria-label="Pairing QR code" v-html="qr" />
        <div class="offer-text">
          <strong>Scan this with Ferry on your other device</strong>
          <p>Or copy the link and paste it into Ferry on another computer. It works once.</p>
          <code class="uri" :title="offer.uri">{{ offer.uri }}</code>
          <p class="waiting" role="status"><i aria-hidden="true" /> Waiting for your other device · <span class="tabular">{{ remaining }}</span></p>
          <div class="row">
            <FButton size="sm" :icon="Copy" @click="copy">Copy link</FButton>
            <FButton size="sm" variant="ghost" :icon="X" @click="hidePairingCode()">Cancel</FButton>
          </div>
        </div>
      </div>

      <div v-else key="start" class="options">
        <div class="option">
          <span class="badge"><FIcon :icon="QrCode" :size="18" /></span>
          <div>
            <strong>Show a pairing code</strong>
            <p>Scan it from your phone, or paste its link on another computer.</p>
          </div>
          <FButton variant="primary" :disabled="showing" @click="show">Show code</FButton>
        </div>
        <form class="option" @submit.prevent="usePairLink">
          <span class="badge"><FIcon :icon="Link2" :size="18" /></span>
          <div class="grow">
            <label for="pair-link"><strong>Have a link from another device?</strong></label>
            <input id="pair-link" v-model="link" class="input" placeholder="ferry://pair?…" autocomplete="off" spellcheck="false" />
          </div>
          <FButton type="submit" :disabled="!link.trim().startsWith('ferry://pair?') || pairing">{{ pairing ? "Pairing…" : "Pair" }}</FButton>
        </form>
        <p class="hint">Or choose <strong>Pair</strong> next to a nearby Ferry device to compare a 6-digit code instead.</p>
      </div>
    </Transition>
  </section>
</template>

<style scoped>
.options {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.option {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  border-radius: var(--radius-md);
  background: var(--fill-1);
}
.option > div {
  flex: 1;
  min-width: 0;
}
.option strong {
  font-size: var(--text-md);
  font-weight: 600;
}
.option p {
  font-size: var(--text-sm);
  color: var(--text-3);
}
.option .input {
  width: 100%;
  margin-top: 6px;
  font-family: var(--font-mono);
  font-size: var(--text-sm);
}
.badge {
  display: grid;
  flex: none;
  place-items: center;
  width: 40px;
  height: 40px;
  border-radius: var(--radius-sm);
  background: var(--accent-softer);
  color: var(--accent-strong);
}
.hint {
  margin-top: var(--space-1);
  font-size: var(--text-sm);
  color: var(--text-3);
}
.hint strong {
  color: var(--text-2);
  font-weight: 600;
}
.offer {
  display: flex;
  align-items: center;
  gap: var(--space-5);
}
.qr {
  flex: none;
  width: 176px;
  height: 176px;
  padding: 6px;
  border-radius: var(--radius-lg);
  background: #fff;
  box-shadow: inset 0 0 0 1px rgb(0 0 0 / 0.06), var(--shadow-1);
}
.qr :deep(svg) {
  display: block;
  width: 100%;
  height: 100%;
}
.offer-text {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}
.offer-text strong {
  font-size: var(--text-lg);
  font-weight: 650;
  letter-spacing: -0.01em;
}
.offer-text p {
  font-size: var(--text-sm);
  color: var(--text-2);
}
.uri {
  display: block;
  max-width: 420px;
  overflow: hidden;
  padding: 6px 10px;
  border-radius: var(--radius-sm);
  background: var(--fill-2);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--text-2);
  white-space: nowrap;
  text-overflow: ellipsis;
  user-select: all;
}
.waiting {
  display: flex;
  align-items: center;
  gap: 8px;
}
.waiting i {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--accent);
  animation: breathe 1.6s var(--ease-in-out) infinite;
}
:root[data-motion="reduced"] .waiting i {
  animation: none;
}
@keyframes breathe {
  50% {
    opacity: 0.35;
  }
}
.row {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-2);
}
.swap-enter-active,
.swap-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--dur-small) var(--ease-out);
}
.swap-enter-from,
.swap-leave-to {
  opacity: 0;
  transform: translateY(calc(6px * var(--motion-scale)));
}
@media (max-width: 639px) {
  .option {
    flex-wrap: wrap;
  }
  .offer {
    flex-direction: column;
    align-items: flex-start;
  }
}
</style>
