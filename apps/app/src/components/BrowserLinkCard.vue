<script setup lang="ts">
// One browser link: QR code, address, live activity, expiry, and Stop.
import { computed, onBeforeUnmount, ref } from "vue";
import { renderSVG } from "uqr";
import { Copy, Download, KeyRound, Upload, X } from "@lucide/vue";
import type { BrowserLink } from "../platform";
import { platform } from "../platform";
import { formatBytes, plural, relativeTime } from "../lib/format";
import { toast } from "../stores/engine";
import { linkPins, stopLink } from "../stores/links";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";

const props = defineProps<{ link: BrowserLink }>();

const url = computed(() => props.link.urls[0] ?? "");
// Scanners need dark-on-light, so the code keeps its white tile in dark mode.
const qr = computed(() => (url.value ? renderSVG(url.value, { border: 1 }) : ""));
// The engine lists browsers oldest first; show the latest.
const clients = computed(() => [...props.link.recentClients].reverse().slice(0, 3));
const pin = computed(() => linkPins.get(props.link.id) ?? null);

const now = ref(Date.now());
const timer = setInterval(() => (now.value = Date.now()), 20_000);
onBeforeUnmount(() => clearInterval(timer));

const title = computed(() => (props.link.kind === "download" ? "Download link" : "Upload link"));
const subtitle = computed(() =>
  props.link.kind === "download"
    ? `${plural(props.link.fileCount, "item")} · ${formatBytes(props.link.totalBytes)}`
    : "Browsers can send files to this device",
);
const activity = computed(() => {
  const l = props.link;
  if (l.active > 0) return l.kind === "download" ? `Downloading now · ${l.active}` : `Uploading now · ${l.active}`;
  if (l.kind === "download" && l.downloads > 0) return `Downloaded ${l.downloads === 1 ? "once" : `${l.downloads} times`}`;
  if (l.kind === "upload" && l.uploads > 0) return `Received ${plural(l.uploads, "file")}`;
  if (l.recentClients.length) return "Opened";
  return "Waiting for someone to open it";
});

async function copy() {
  await platform.copyText(url.value);
  toast({ level: "success", title: "Link copied" }, 1500);
}
</script>

<template>
  <article class="link-card" :aria-label="title">
    <header class="head">
      <span class="badge" :class="link.kind"><FIcon :icon="link.kind === 'download' ? Download : Upload" :size="16" /></span>
      <div class="titles">
        <strong>{{ title }}</strong>
        <span class="tabular">{{ subtitle }}</span>
      </div>
      <FButton variant="ghost" size="sm" :icon="X" @click="stopLink(link.id)">Stop</FButton>
    </header>

    <div class="body">
      <!-- SVG generated locally by uqr from our own URL -->
      <div class="qr" role="img" :aria-label="`QR code for ${url}`" v-html="qr" />
      <div class="details">
        <div class="url">
          <code :title="url">{{ url.replace(/^https?:\/\//, "") }}</code>
          <FButton variant="ghost" size="sm" icon-only :icon="Copy" label="Copy link" @click="copy" />
        </div>
        <p class="activity" :class="{ live: link.active > 0 }" role="status">
          <i aria-hidden="true" />{{ activity }}
        </p>
        <ul v-if="clients.length" class="clients" aria-label="Opened by">
          <li v-for="c in clients" :key="c">{{ c }}</li>
        </ul>
        <p v-if="pin" class="pin"><FIcon :icon="KeyRound" :size="13" /> PIN <code class="tabular">{{ pin }}</code></p>
        <p v-else-if="link.pinRequired" class="pin"><FIcon :icon="KeyRound" :size="13" /> PIN required</p>
        <p class="fine">
          Same network only · not encrypted · expires {{ relativeTime(link.expiresAtMs, now) }}
        </p>
      </div>
    </div>
  </article>
</template>

<style scoped>
.link-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding: var(--space-3);
  border: 1px solid var(--hairline);
  border-radius: var(--radius-lg);
  background: var(--fill-1);
}
.head {
  display: flex;
  align-items: center;
  gap: 10px;
}
.badge {
  display: grid;
  flex: none;
  place-items: center;
  width: 32px;
  height: 32px;
  border-radius: var(--radius-sm);
  background: var(--accent-softer);
  color: var(--accent-strong);
}
.badge.upload {
  background: var(--ok-soft);
  color: var(--ok);
}
.titles {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
}
.titles strong {
  font-size: var(--text-md);
  font-weight: 620;
}
.titles span {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.body {
  display: flex;
  gap: var(--space-3);
  align-items: flex-start;
}
.qr {
  flex: none;
  width: 112px;
  height: 112px;
  padding: 4px;
  border-radius: var(--radius-md);
  background: #fff;
  box-shadow: inset 0 0 0 1px rgb(0 0 0 / 0.06);
}
.qr :deep(svg) {
  display: block;
  width: 100%;
  height: 100%;
}
.details {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}
.url {
  display: flex;
  align-items: center;
  gap: 4px;
  padding-left: 10px;
  border-radius: var(--radius-sm);
  background: var(--fill-2);
}
.url code {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  white-space: nowrap;
  text-overflow: ellipsis;
  user-select: all;
}
.activity {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--text-sm);
  font-weight: 560;
  color: var(--text-2);
}
.activity i {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text-3);
}
.activity.live {
  color: var(--text-1);
}
.activity.live i {
  background: var(--ok);
  box-shadow: 0 0 0 3px var(--ok-soft);
}
.clients {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.clients li {
  padding: 2px 8px;
  border-radius: var(--radius-pill);
  background: var(--fill-2);
  font-size: var(--text-xs);
  color: var(--text-2);
}
.pin {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--text-sm);
  color: var(--text-2);
}
.pin code {
  font-family: var(--font-mono);
  font-weight: 600;
  letter-spacing: 0.14em;
  color: var(--text-1);
}
.fine {
  font-size: var(--text-xs);
  color: var(--text-3);
}
@media (max-width: 379px) {
  .body {
    flex-direction: column;
    align-items: center;
  }
  .details {
    width: 100%;
  }
}
</style>
