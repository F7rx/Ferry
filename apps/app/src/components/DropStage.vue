<script setup lang="ts">
// The drop surface: a central well with nearby devices settled around it.
// idle → drag over (well + tiles react) → items staged (chips, selectable
// devices, send bar) → send (chips fly to their devices).
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { ArrowDownToLine, ArrowRight, ClipboardPaste, FilePlus2, FolderPlus, Link2, Plus, Type, X } from "@lucide/vue";
import { compose, clearCompose, hasFolders, sendStaged, stage, stagedBytes, toggleTarget, unstage } from "../stores/compose";
import { devices as allDevices, displayName, store, toast } from "../stores/engine";
import { shareByLink } from "../stores/links";
import type { OutgoingItem } from "../platform";
import { platform } from "../platform";
import { formatBytes, plural } from "../lib/format";
import { fly, pulse } from "../lib/motion";
import DeviceTile from "./DeviceTile.vue";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";
import FileChip from "./FileChip.vue";
import { useRouter } from "vue-router";

const router = useRouter();
const stageEl = ref<HTMLElement | null>(null);
const width = ref(0);
const height = ref(0);
let observer: ResizeObserver | null = null;

onMounted(() => {
  // Applied on the next frame: resizing tiles from inside the observer
  // callback would trigger "ResizeObserver loop" warnings.
  let frame = 0;
  observer = new ResizeObserver(([entry]) => {
    if (!entry) return;
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => {
      width.value = entry.contentRect.width;
      height.value = entry.contentRect.height;
    });
  });
  if (stageEl.value) observer.observe(stageEl.value);
});
onBeforeUnmount(() => observer?.disconnect());

const orbit = computed(() => width.value >= 640);
const staged = computed(() => compose.items.length > 0);
// Online devices, plus remembered favorites so Quick Drop targets stay put.
const candidates = computed(() => allDevices.value.filter((d) => d.online || d.favorite || d.mine));

// Preferred spots on an ellipse around the well (degrees, 0 = right, 90 = down).
const ANGLES = [210, 330, 150, 30, 270, 90, 245, 295, 115, 65, 180, 0];
const TILE_W = 132;
const TILE_H = 120;
const WELL_CLEARANCE = 100; // well radius + breathing room
const GAP = 10;

/** Greedy placement: each device takes the first preferred spot that doesn't
 * overlap another tile, the well, or the stage edge. */
const placements = computed(() => {
  if (!orbit.value) return [];
  const w = width.value;
  const h = height.value;
  const cx = w / 2;
  const cy = h / 2;
  const rx = Math.min(cx - TILE_W / 2 - 12, 340);
  const ry = Math.min(cy - TILE_H / 2 - 4, 178);
  const placed: { x: number; y: number }[] = [];
  for (let i = 0; i < candidates.value.length; i++) {
    let spot: { x: number; y: number } | null = null;
    for (const deg of ANGLES) {
      const a = (deg * Math.PI) / 180;
      const x = cx + rx * Math.cos(a) - TILE_W / 2;
      const y = cy + ry * Math.sin(a) - TILE_H / 2;
      if (x < 0 || y < -4 || x + TILE_W > w || y + TILE_H > h + 4) continue;
      const overlaps = placed.some((p) => Math.abs(p.x - x) < TILE_W + GAP && Math.abs(p.y - y) < TILE_H + GAP);
      // Distance from the well's centre to the nearest point of the tile.
      const nx = Math.max(x, Math.min(cx, x + TILE_W));
      const ny = Math.max(y, Math.min(cy, y + TILE_H));
      const hitsWell = Math.hypot(nx - cx, ny - cy) < WELL_CLEARANCE;
      if (!overlaps && !hitsWell) {
        spot = { x, y };
        break;
      }
    }
    if (!spot) break;
    placed.push(spot);
  }
  return placed;
});
const shown = computed(() => (orbit.value ? candidates.value.slice(0, placements.value.length) : candidates.value.slice(0, 12)));
const overflow = computed(() => allDevices.value.filter((d) => d.online).length - shown.value.filter((d) => d.online).length);

function slotStyle(index: number) {
  const p = placements.value[index] ?? { x: 0, y: 0 };
  // `translate`, not `transform`: TransitionGroup's FLIP rewrites `transform`.
  return { translate: `${Math.round(p.x)}px ${Math.round(p.y)}px`, "--i": index };
}

async function chooseFiles() {
  const items = await platform.pickFiles();
  if (items) stage(items);
}
async function chooseFolder() {
  const items = await platform.pickFolder();
  if (items) stage(items);
}
async function paste() {
  const item = await platform.readClipboard();
  if (item) stage([item]);
}

const textOpen = ref(false);
const text = ref("");
const textArea = ref<HTMLTextAreaElement | null>(null);
async function openText() {
  textOpen.value = true;
  await nextTick();
  textArea.value?.focus();
}
function addText() {
  const value = text.value.trim();
  if (value) stage([{ kind: "text", text: value, name: "Text" }]);
  text.value = "";
  textOpen.value = false;
}

function activate(id: string) {
  if (staged.value) toggleTarget(id);
  else void quickPick(id);
}

/** Clicking a device with nothing staged: pick files and send right away. */
async function quickPick(id: string) {
  const items = await platform.pickFiles();
  if (!items?.length) return;
  stage(items);
  compose.targets.clear();
  compose.targets.add(id);
}

const sending = ref(false);
const targetNames = computed(() => [...compose.targets].map((id) => store.devices.get(id)).filter(Boolean).map((d) => displayName(d!)));
const sendLabel = computed(() => {
  const names = targetNames.value;
  if (names.length === 0) return "Choose a device";
  if (names.length === 1) return `Send to ${names[0]}`;
  return `Send to ${names.length} devices`;
});

// Browser links carry files and folders; text goes to devices only.
const linkable = computed(() => compose.items.filter((i) => i.kind !== "text"));
const sharing = ref(false);
async function shareLink() {
  if (!linkable.value.length || sharing.value) return;
  sharing.value = true;
  const skipped = compose.items.length - linkable.value.length;
  const link = await shareByLink(linkable.value.map(({ key: _k, ...rest }) => rest as OutgoingItem));
  sharing.value = false;
  if (!link) return;
  clearCompose();
  toast(
    { level: "success", title: "Link ready", body: skipped ? "Scan the code from any browser on this network. Text isn't included in links." : "Scan the code from any browser on this network." },
    3600,
  );
}

async function send() {
  if (!compose.targets.size || sending.value) return;
  sending.value = true;
  const chips = [...(stageEl.value?.querySelectorAll<HTMLElement>(".chips .chip") ?? [])].slice(0, 6);
  const tiles = [...compose.targets]
    .map((id) => stageEl.value?.querySelector<HTMLElement>(`[data-device-id="${CSS.escape(id)}"]`))
    .filter((t): t is HTMLElement => !!t);
  const flights = tiles.flatMap((tile, t) => chips.map((chip, c) => fly(chip, tile, c * 45 + t * 70)));
  const result = sendStaged();
  await Promise.all(flights);
  tiles.forEach((t) => pulse(t));
  await result;
  sending.value = false;
}
</script>

<template>
  <div class="drop-stage" :class="{ orbit, staged, dragging: compose.dragging }">
    <!-- Staged items -->
    <Transition name="tray">
      <div v-if="staged" class="tray">
        <TransitionGroup tag="div" name="chip" class="chips" role="list" aria-label="Ready to send">
          <FileChip v-for="item in compose.items" :key="item.key" :item="item" role="listitem" @remove="unstage(item.key)" />
        </TransitionGroup>
        <div class="tray-summary">
          <span class="tabular">
            {{ plural(compose.items.length, "item") }}<template v-if="stagedBytes"> · {{ formatBytes(stagedBytes) }}</template>
            <template v-if="hasFolders"> · plus folder contents</template>
          </span>
          <FButton size="sm" variant="ghost" :icon="X" @click="clearCompose()">Clear</FButton>
        </div>
      </div>
    </Transition>

    <div ref="stageEl" class="stage">
      <!-- The well -->
      <div class="drop-well" :class="{ hot: compose.dragging && !compose.hoverTarget }">
        <div class="rings" aria-hidden="true"><i /><i /><i /></div>
        <button type="button" class="core" :aria-label="staged ? 'Add more files' : 'Choose files to send'" @click="chooseFiles">
          <FIcon :icon="staged ? Plus : ArrowDownToLine" :size="26" :stroke="1.6" />
          <strong v-if="compose.dragging">Release to add</strong>
          <strong v-else-if="staged">Add more</strong>
          <strong v-else>Drop files here</strong>
          <span v-if="!compose.dragging">{{ staged ? "or pick devices around" : "or click to choose" }}</span>
          <span v-else>or onto a device to send now</span>
        </button>
      </div>

      <!-- Devices -->
      <template v-if="orbit">
        <TransitionGroup name="settle">
          <div v-for="(d, i) in shown" :key="d.id" class="slot" :style="slotStyle(i)">
            <DeviceTile
              :device="d"
              :selectable="staged"
              :selected="compose.targets.has(d.id)"
              :receptive="compose.dragging"
              :targeted="compose.hoverTarget === d.id"
              @activate="activate(d.id)"
            />
          </div>
        </TransitionGroup>
      </template>
      <div v-else class="grid">
        <TransitionGroup name="settle">
          <DeviceTile
            v-for="d in shown"
            :key="d.id"
            compact
            :device="d"
            :selectable="staged"
            :selected="compose.targets.has(d.id)"
            :receptive="compose.dragging"
            :targeted="compose.hoverTarget === d.id"
            @activate="activate(d.id)"
          />
        </TransitionGroup>
      </div>

      <p v-if="!shown.length" class="searching" role="status">
        Looking for nearby devices…
        <button type="button" class="link" @click="router.push('/diagnostics')">Not seeing one?</button>
      </p>
      <button v-else-if="overflow > 0" type="button" class="more" @click="router.push('/devices')">+{{ overflow }} more</button>
    </div>

    <!-- Actions -->
    <div class="actions">
      <Transition name="swap" mode="out-in">
        <div v-if="staged" key="send" class="send-bar">
          <span class="hint">{{ compose.targets.size ? "" : "Select one or more devices" }}</span>
          <FButton
            v-if="platform.capabilities.browserLinks"
            size="lg"
            :icon="Link2"
            :disabled="!linkable.length || sharing"
            :title="linkable.length ? 'Anyone on this network can download with a browser' : 'Links can carry files and folders'"
            @click="shareLink"
          >
            Share by link
          </FButton>
          <FButton variant="primary" size="lg" :icon="ArrowRight" :disabled="!compose.targets.size || sending" @click="send">
            {{ sendLabel }}
          </FButton>
        </div>
        <div v-else key="pick" class="pickers">
          <FButton :icon="FilePlus2" @click="chooseFiles">Files</FButton>
          <FButton :icon="FolderPlus" @click="chooseFolder">Folder</FButton>
          <FButton :icon="Type" @click="openText">Text or link</FButton>
          <FButton :icon="ClipboardPaste" @click="paste">Paste</FButton>
        </div>
      </Transition>
    </div>

    <Transition name="pop">
      <form v-if="textOpen" class="text-composer glass-2" @submit.prevent="addText" @keydown.esc="textOpen = false">
        <label for="compose-text" class="eyebrow">Text or link</label>
        <textarea id="compose-text" ref="textArea" v-model="text" rows="3" placeholder="Type or paste something to send" @keydown.enter.exact.prevent="addText" />
        <div class="row">
          <FButton size="sm" variant="ghost" @click="textOpen = false">Cancel</FButton>
          <FButton size="sm" variant="primary" type="submit" :disabled="!text.trim()">Add</FButton>
        </div>
      </form>
    </Transition>
  </div>
</template>

<style scoped>
.drop-stage {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}

/* ── Tray ── */
.tray {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.chips {
  display: flex;
  gap: var(--space-2);
  overflow-x: auto;
  padding: 2px 2px 6px;
  scrollbar-width: thin;
}
.chips > * {
  flex: none;
}
.tray-summary {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: var(--text-sm);
  color: var(--text-3);
}

/* ── Stage ── */
.stage {
  position: relative;
  min-height: 448px;
}
.drop-stage:not(.orbit) .stage {
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
}
.drop-well {
  position: absolute;
  inset: 0;
  display: grid;
  place-items: center;
  pointer-events: none;
}
.drop-stage:not(.orbit) .drop-well {
  position: relative;
  height: 240px;
}
.rings {
  position: absolute;
  display: grid;
  place-items: center;
}
.rings i {
  position: absolute;
  border-radius: 50%;
  border: 1px solid var(--hairline);
  transition:
    border-color var(--dur-panel) var(--ease-out),
    transform var(--spring-soft-dur) var(--spring-soft),
    opacity var(--dur-panel) var(--ease-out);
}
.rings i:nth-child(1) {
  width: 236px;
  height: 236px;
}
.rings i:nth-child(2) {
  width: 330px;
  height: 330px;
  opacity: 0.7;
}
.rings i:nth-child(3) {
  width: 440px;
  height: 440px;
  opacity: 0.4;
}
.drop-stage:not(.orbit) .rings i:nth-child(3) {
  display: none;
}
.drop-well.hot .rings i {
  border-color: color-mix(in srgb, var(--accent) 45%, transparent);
  transform: scale(calc(1 + 0.04 * var(--motion-scale)));
}
.core {
  pointer-events: auto;
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 4px;
  width: 176px;
  height: 176px;
  border-radius: 50%;
  border: 1px solid var(--glass-rim);
  background:
    radial-gradient(circle at 50% 30%, color-mix(in srgb, var(--solid-1) 92%, transparent), transparent 70%),
    var(--glass-2);
  box-shadow:
    inset 0 1px 0 var(--glass-edge),
    inset 0 -18px 36px -24px var(--accent-glow),
    var(--shadow-2);
  color: var(--accent-strong);
  transition:
    transform var(--spring-soft-dur) var(--spring-soft),
    box-shadow var(--dur-small) var(--ease-out);
}
:root[data-theme="dark"] .core {
  color: var(--accent-text);
}
.core strong {
  margin-top: 6px;
  font-size: var(--text-md);
  font-weight: 620;
  color: var(--text-1);
  letter-spacing: -0.01em;
}
.core span {
  font-size: var(--text-xs);
  color: var(--text-3);
}
@media (hover: hover) {
  .core:hover {
    transform: scale(calc(1 + 0.02 * var(--motion-scale)));
  }
}
.drop-well.hot .core {
  transform: scale(calc(1 + 0.07 * var(--motion-scale)));
  box-shadow:
    inset 0 1px 0 var(--glass-edge),
    0 0 0 6px var(--accent-soft),
    0 24px 50px -18px var(--accent-glow);
}

.slot {
  position: absolute;
  top: 0;
  left: 0;
  transition: translate var(--spring-soft-dur) var(--spring-soft);
}
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(116px, 1fr));
  gap: var(--space-3);
  justify-items: center;
}
.searching {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 8px;
  text-align: center;
  font-size: var(--text-sm);
  color: var(--text-3);
}
.drop-stage:not(.orbit) .searching {
  position: static;
}
.link {
  border: 0;
  background: none;
  padding: 0 4px;
  color: var(--accent-text);
  font-weight: 560;
}
.more {
  position: absolute;
  right: 0;
  bottom: 0;
  height: 32px;
  padding: 0 12px;
  border: 1px solid var(--hairline);
  border-radius: var(--radius-pill);
  background: var(--glass-2);
  color: var(--text-2);
  font-size: var(--text-sm);
  font-weight: 560;
}

/* ── Actions ── */
.actions {
  display: flex;
  justify-content: center;
  min-height: 48px;
}
.pickers {
  display: flex;
  flex-wrap: wrap;
  justify-content: center;
  gap: var(--space-2);
}
.send-bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: center;
  gap: var(--space-2) var(--space-3);
}
.hint {
  font-size: var(--text-sm);
  color: var(--text-3);
}

.text-composer {
  position: absolute;
  left: 50%;
  bottom: 64px;
  z-index: 5;
  width: min(440px, 100%);
  padding: var(--space-4);
  border-radius: var(--radius-lg);
  transform: translateX(-50%);
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.text-composer textarea {
  width: 100%;
  resize: vertical;
  min-height: 84px;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--hairline-strong);
  background: var(--solid-1);
  font-size: var(--text-base);
  line-height: 1.45;
}
.text-composer textarea:focus-visible {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.text-composer .row {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}

/* ── Motion ── */
.settle-enter-active {
  transition:
    opacity var(--spring-soft-dur) var(--ease-out),
    scale var(--spring-soft-dur) var(--spring-soft);
  transition-delay: calc(var(--i, 0) * 60ms);
}
.settle-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-in),
    scale var(--dur-small) var(--ease-in);
}
.settle-enter-from,
.settle-leave-to {
  opacity: 0;
  scale: calc(1 - 0.08 * var(--motion-scale));
}
.chip-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-settle-dur) var(--spring-settle),
    filter var(--dur-panel) var(--ease-out);
}
.chip-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-in),
    transform var(--dur-small) var(--ease-in);
}
.chip-enter-from {
  opacity: 0;
  transform: translateY(calc(10px * var(--motion-scale))) scale(calc(1 - 0.08 * var(--motion-scale)));
  filter: blur(calc(4px * var(--motion-scale)));
}
.chip-leave-to {
  opacity: 0;
  transform: scale(calc(1 - 0.1 * var(--motion-scale)));
}
.chip-move {
  transition: transform var(--dur-panel) var(--ease-out);
}
.tray-enter-active,
.tray-leave-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--dur-panel) var(--ease-out);
}
.tray-enter-from,
.tray-leave-to {
  opacity: 0;
  transform: translateY(calc(-8px * var(--motion-scale)));
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
.pop-enter-active,
.pop-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--spring-snappy-dur) var(--spring-snappy);
}
.pop-enter-from,
.pop-leave-to {
  opacity: 0;
  transform: translateX(-50%) translateY(calc(8px * var(--motion-scale))) scale(calc(1 - 0.03 * var(--motion-scale)));
}
</style>
