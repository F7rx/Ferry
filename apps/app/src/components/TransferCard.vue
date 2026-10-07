<script setup lang="ts">
import { computed, ref } from "vue";
import { ArrowDownLeft, ArrowUpRight, Check, ChevronDown, CircleAlert, FolderOpen, KeyRound, Lock, LockOpen, Pause, Play, RotateCw, X } from "@lucide/vue";
import type { TransferSummary } from "../platform";
import { platform } from "../platform";
import { attempt, loadFiles, store, toast } from "../stores/engine";
import { fileIcon } from "../lib/icons";
import { formatBytes, formatEta, formatSpeed } from "../lib/format";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";
import ProgressRing from "./ProgressRing.vue";

const props = defineProps<{ transfer: TransferSummary }>();
const t = computed(() => props.transfer);
const expanded = ref(false);
const pin = ref("");

const progress = computed(() => (t.value.totalBytes ? t.value.bytesDone / t.value.totalBytes : t.value.state === "completed" ? 1 : 0));
const done = computed(() => t.value.state === "completed");
const failed = computed(() => ["failed", "declined", "cancelled"].includes(t.value.state));
// Only real errors are red; a decline or a cancel is an outcome, not a fault.
const errored = computed(() => t.value.state === "failed");
const final = computed(() => done.value || failed.value || t.value.state === "completedWithErrors");
const tone = computed(() => (done.value ? "ok" : errored.value ? "danger" : failed.value ? "muted" : t.value.state === "reconnecting" || t.value.state === "completedWithErrors" ? "warn" : t.value.state === "paused" ? "muted" : "accent"));

const title = computed(() => {
  if (t.value.text) return t.value.text.length > 70 ? `${t.value.text.slice(0, 70)}…` : t.value.text;
  if (t.value.fileCount <= 1) return t.value.title;
  return `${t.value.title} and ${t.value.fileCount - 1} more`;
});
const toFrom = computed(() => `${t.value.direction === "send" ? "To" : "From"} ${t.value.peer.alias}`);
const status = computed(() => {
  const s = t.value;
  switch (s.state) {
    case "preparing":
      return "Preparing…";
    case "waitingForAcceptance":
      return s.error?.code === "receiver_busy" ? `${s.peer.alias} is busy. Waiting…` : `Waiting for ${s.peer.alias} to accept…`;
    case "pinRequired":
      return s.error?.code === "pin_invalid" ? "Wrong PIN. Try again." : `${s.peer.alias} needs a PIN`;
    case "transferring":
      return [formatSpeed(s.speedBps), formatEta(s.etaSecs)].filter(Boolean).join(" · ") || "Starting…";
    case "paused":
      return "Paused";
    case "reconnecting":
      return s.error?.message ?? `Connection lost. Waiting for ${s.peer.alias}…`;
    case "verifying":
      return "Verifying…";
    case "completed":
      return s.text ? "Delivered" : `${formatBytes(s.totalBytes)} · Done`;
    case "completedWithErrors":
      return `${s.filesDone} of ${s.fileCount} done`;
    case "declined":
      return `${s.peer.alias} declined`;
    case "cancelled":
      return s.error?.code === "cancelled_by_peer" ? `Cancelled by ${s.peer.alias}` : "Cancelled";
    case "failed":
      return s.error?.message ?? "Failed";
  }
  return "";
});
const announcement = computed(() => (t.value.state === "transferring" ? `${t.value.direction === "send" ? "Sending" : "Receiving"} ${title.value}` : status.value));
const connection = computed(() => {
  const c = t.value.connection;
  if (!c) return null;
  // WebRTC may well be on the same network; say how it travels, not where.
  // Direct or relayed only when known (null: the route wasn't reported).
  const route = c.relayed === true ? " · Relayed" : c.relayed === false ? " · Direct" : "";
  const where = c.transport === "webrtc" ? `Peer-to-peer${route}` : "Nearby";
  const ip = c.ipVersion ? ` · IPv${c.ipVersion}` : "";
  return { label: `${where}${ip}`, encrypted: c.encrypted };
});
const icon = computed(() => fileIcon(t.value.title, undefined, false));
const showBytes = computed(() => ["transferring", "paused", "reconnecting"].includes(t.value.state) && t.value.totalBytes > 0);
const files = computed(() => store.files.get(t.value.id) ?? []);

async function toggle() {
  expanded.value = !expanded.value;
  if (expanded.value) await loadFiles(t.value.id);
}
const act = (fn: () => Promise<unknown>) => attempt(fn);
/** Runs a pause or resume; `false` means the transfer couldn't do it (errors already show a toast). */
async function control(fn: () => Promise<boolean>, failure: string) {
  if ((await attempt(fn)) === false) toast({ level: "error", title: failure });
}
const pause = () => control(() => platform.pause(t.value.id), "Couldn't pause the transfer");
const resume = () => control(() => platform.resume(t.value.id), "Couldn't resume the transfer");
// Browser app: a send that lost its connection continues where it stopped.
// Never offered for a decline, a cancel or any other failure.
const canRetry = computed(
  () => platform.capabilities.kind === "web" && t.value.direction === "send" && t.value.state === "failed" && t.value.error?.code === "connection_lost",
);
const retrying = ref(false);
async function retry() {
  if (retrying.value) return;
  retrying.value = true;
  try {
    await control(() => platform.resume(t.value.id), "Couldn't try again");
  } finally {
    retrying.value = false;
  }
}
const cancel = () => act(() => platform.cancel(t.value.id));
const dismiss = () => act(() => platform.dismiss(t.value.id));
const reveal = () => t.value.saveDir && act(() => platform.reveal(t.value.saveDir!));
const canReveal = platform.capabilities.revealInFolder;
async function submitPin() {
  if (!pin.value) return;
  await act(() => platform.submitPin(t.value.id, pin.value));
  pin.value = "";
}
</script>

<template>
  <article class="card" :class="[t.state, { final }]" :aria-label="`${toFrom}: ${title}, ${status}`">
    <div class="main">
      <ProgressRing :value="progress" :tone="tone" :size="46" :indeterminate="t.state === 'preparing' || t.state === 'waitingForAcceptance' || t.state === 'verifying'">
        <Transition name="morph" mode="out-in">
          <FIcon v-if="done" key="ok" :icon="Check" :size="20" :stroke="2.4" class="ok" />
          <FIcon v-else-if="failed" key="x" :icon="CircleAlert" :size="18" :class="{ bad: errored }" />
          <FIcon v-else key="type" :icon="icon" :size="18" />
        </Transition>
      </ProgressRing>

      <div class="text">
        <div class="line1">
          <FIcon :icon="t.direction === 'send' ? ArrowUpRight : ArrowDownLeft" :size="14" class="dir" />
          <span class="title">{{ title }}</span>
        </div>
        <div class="line2">{{ toFrom }}</div>
        <div class="line3 tabular">
          <span class="status" :class="{ warn: t.state === 'reconnecting', bad: errored }">{{ status }}</span>
          <span v-if="showBytes"> · {{ formatBytes(t.bytesDone) }} of {{ formatBytes(t.totalBytes) }}</span>
        </div>
      </div>

      <div class="buttons">
        <template v-if="!final">
          <FButton v-if="t.canPause && t.state === 'transferring'" variant="ghost" size="sm" icon-only :icon="Pause" label="Pause" @click="pause" />
          <FButton v-if="t.canResume && t.state === 'paused'" variant="ghost" size="sm" icon-only :icon="Play" label="Resume" @click="resume" />
          <FButton variant="ghost" size="sm" icon-only :icon="X" label="Cancel" @click="cancel" />
        </template>
        <template v-else>
          <FButton v-if="canRetry" variant="ghost" size="sm" icon-only :icon="RotateCw" label="Try again" :disabled="retrying" @click="retry" />
          <FButton
            v-if="done && t.direction === 'receive' && t.saveDir && canReveal"
            variant="ghost"
            size="sm"
            icon-only
            :icon="FolderOpen"
            label="Show in folder"
            @click="reveal"
          />
          <FButton variant="ghost" size="sm" icon-only :icon="X" label="Dismiss" @click="dismiss" />
        </template>
        <FButton v-if="t.fileCount > 1" variant="ghost" size="sm" icon-only :icon="ChevronDown" :label="expanded ? 'Hide files' : 'Show files'" :class="{ flipped: expanded }" @click="toggle" />
      </div>
    </div>

    <!-- Announces state changes only; the speed line would repeat every tick. -->
    <span class="visually-hidden" role="status">{{ announcement }}</span>

    <form v-if="t.state === 'pinRequired'" class="pin" @submit.prevent="submitPin">
      <FIcon :icon="KeyRound" :size="16" />
      <input v-model="pin" inputmode="numeric" autocomplete="one-time-code" :placeholder="`PIN shown on ${t.peer.alias}`" aria-label="PIN" />
      <FButton size="sm" variant="primary" type="submit" :disabled="!pin">Send PIN</FButton>
    </form>

    <p v-if="t.error?.hint && (failed || t.state === 'reconnecting')" class="hint">{{ t.error.hint }}</p>

    <p v-if="connection && !final" class="conn" :class="{ insecure: !connection.encrypted }">
      <FIcon :icon="connection.encrypted ? Lock : LockOpen" :size="12" />
      <span>{{ connection.encrypted ? "Encrypted" : "Not encrypted" }} · {{ connection.label }}<template v-if="t.resumable"> · Resumable</template></span>
    </p>

    <Transition name="expand">
      <ul v-if="expanded" class="files" aria-label="Files">
        <li v-for="f in files.slice(0, 200)" :key="f.id" :class="f.state">
          <FIcon :icon="fileIcon(f.name, f.mime)" :size="14" />
          <span class="fname">{{ f.name }}</span>
          <span class="fsize tabular">{{ f.state === "transferring" ? `${Math.round((f.bytesDone / Math.max(1, f.size)) * 100)}%` : formatBytes(f.size) }}</span>
          <FIcon v-if="f.state === 'done'" :icon="Check" :size="14" class="ok" />
          <FIcon v-else-if="f.state === 'failed'" :icon="CircleAlert" :size="14" class="bad" :title="f.error?.message" />
        </li>
        <li v-if="files.length > 200" class="more">and {{ files.length - 200 }} more</li>
      </ul>
    </Transition>
  </article>
</template>

<style scoped>
.card {
  padding: 14px 14px 12px;
  border-radius: var(--radius-lg);
  background: var(--solid-1);
  box-shadow:
    inset 0 0 0 1px var(--hairline),
    var(--shadow-1);
}
.main {
  display: flex;
  align-items: center;
  gap: 12px;
}
.text {
  flex: 1;
  min-width: 0;
}
.line1 {
  display: flex;
  align-items: center;
  gap: 6px;
}
.dir {
  color: var(--text-3);
}
.title {
  overflow: hidden;
  font-size: var(--text-md);
  font-weight: 600;
  letter-spacing: -0.01em;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.line2,
.line3 {
  margin-top: 1px;
  overflow: hidden;
  font-size: var(--text-sm);
  color: var(--text-3);
  white-space: nowrap;
  text-overflow: ellipsis;
}
.line3 {
  font-size: var(--text-xs);
}
.status.warn {
  color: var(--warn);
}
.status.bad {
  color: var(--danger);
}
.buttons {
  display: flex;
  gap: 2px;
}
.flipped :deep(svg) {
  transform: rotate(180deg);
}
.ok {
  color: var(--ok);
}
.bad {
  color: var(--danger);
}
.pin {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
  padding: 6px 6px 6px 12px;
  border-radius: var(--radius-pill);
  background: var(--fill-1);
  color: var(--text-3);
}
.pin input {
  flex: 1;
  min-width: 0;
  height: 32px;
  border: 0;
  background: transparent;
  font-size: var(--text-md);
  letter-spacing: 0.12em;
}
.pin input:focus {
  outline: none;
}
.hint {
  margin-top: 8px;
  font-size: var(--text-sm);
  color: var(--text-3);
}
.conn {
  display: flex;
  align-items: center;
  gap: 5px;
  margin-top: 8px;
  padding-left: 58px;
  overflow: hidden;
  font-size: var(--text-2xs);
  font-weight: 540;
  color: var(--text-3);
  white-space: nowrap;
}
.conn span {
  overflow: hidden;
  text-overflow: ellipsis;
}
.conn.insecure {
  color: var(--warn);
}
.files {
  list-style: none;
  margin: 10px 0 0;
  padding: 8px 0 0 58px;
  border-top: 1px solid var(--hairline);
  max-height: 240px;
  overflow: auto;
}
.files li {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 28px;
  font-size: var(--text-sm);
  color: var(--text-2);
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
.files li.skipped {
  opacity: 0.5;
}
.more {
  color: var(--text-3);
}
.morph-enter-active,
.morph-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--spring-snappy-dur) var(--spring-snappy);
}
.morph-enter-from,
.morph-leave-to {
  opacity: 0;
  transform: scale(calc(1 - 0.4 * var(--motion-scale)));
}
.expand-enter-active,
.expand-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--dur-small) var(--ease-out);
}
.expand-enter-from,
.expand-leave-to {
  opacity: 0;
  transform: translateY(calc(-4px * var(--motion-scale)));
}
</style>
