<script setup lang="ts">
// Private links ("rooms"). Opening the same link puts two devices in touch on
// any network; the secret stays in the URL fragment and every connection
// proves it, on top of the device identity check. Needs a signaling server.
import { useRouter } from "vue-router";
import { computed, ref } from "vue";
import { renderSVG } from "uqr";
import { Copy, Link2, X } from "@lucide/vue";
import { platform } from "../platform";
import { attempt, store, toast } from "../stores/engine";
import FButton from "./FButton.vue";
import FIcon from "./FIcon.vue";

const router = useRouter();
const rooms = computed(() => [...store.rooms.values()].sort((a, b) => b.createdAtMs - a.createdAtMs));
/** The desktop app needs a signaling server before links can work. */
const needsServer = computed(() => store.signaling?.state === "off" || (store.signaling != null && !store.signaling.url));
const qr = (link: string) => renderSVG(link, { border: 1 });

async function create() {
  const room = await attempt(() => platform.createRoom());
  if (room) store.rooms.set(room.id, room);
}
async function copy(link: string) {
  await platform.copyText(link);
  toast({ level: "success", title: "Link copied", body: "Send it to your other device. Anyone with the link can see this device while it's open." }, 3600);
}
async function stop(id: string) {
  await attempt(() => platform.leaveRoom(id));
  store.rooms.delete(id);
}
const pasted = ref("");
async function join() {
  const room = await attempt(() => platform.joinRoom(pasted.value));
  if (room) {
    store.rooms.set(room.id, room);
    pasted.value = "";
  }
}
</script>

<template>
  <section class="module glass" aria-labelledby="rooms-title">
    <div class="module-head">
      <h2 id="rooms-title">Beyond this network</h2>
      <FIcon :icon="Link2" :size="16" class="muted" />
    </div>

    <template v-if="needsServer">
      <p class="lede">To reach devices on other networks, set up a signaling server. It only introduces devices; files go directly between them.</p>
      <FButton :icon="Link2" @click="router.push('/settings/network')">Set up</FButton>
    </template>
    <TransitionGroup v-else-if="rooms.length" tag="div" name="stack" class="rooms">
      <article v-for="r in rooms" :key="r.id" class="room" aria-label="Private link">
        <!-- SVG generated locally by uqr from our own link -->
        <div class="qr" role="img" aria-label="QR code for the private link" v-html="qr(r.link)" />
        <div class="details">
          <div class="url">
            <code :title="r.link">{{ r.link.replace(/^https?:\/\//, "") }}</code>
            <FButton variant="ghost" size="sm" icon-only :icon="Copy" label="Copy link" @click="copy(r.link)" />
          </div>
          <p class="status" :class="{ live: r.peers > 0 }" role="status">
            <i aria-hidden="true" />{{ r.peers ? `${r.peers} ${r.peers === 1 ? "device" : "devices"} connected` : "Waiting for someone to open it" }}
          </p>
          <p class="fine">Open it on your other device. Works across networks when a direct connection is possible.</p>
          <FButton size="sm" variant="ghost" :icon="X" @click="stop(r.id)">Close link</FButton>
        </div>
      </article>
    </TransitionGroup>
    <template v-else>
      <p class="lede">Your other device isn't on this network? Create a private link and open it there.</p>
      <FButton variant="primary" :icon="Link2" @click="create">Create a link</FButton>
    </template>

    <form v-if="!needsServer" class="paste" @submit.prevent="join">
      <input v-model="pasted" class="input" placeholder="Paste a Ferry link" aria-label="Paste a Ferry link" spellcheck="false" autocomplete="off" />
      <FButton type="submit" size="sm" :disabled="!pasted.includes('room=')">Join</FButton>
    </form>
  </section>
</template>

<style scoped>
.muted {
  color: var(--text-3);
}
.lede {
  margin-bottom: var(--space-3);
  font-size: var(--text-sm);
  color: var(--text-2);
}
.rooms {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.room {
  display: flex;
  gap: var(--space-3);
  padding: var(--space-3);
  border: 1px solid var(--hairline);
  border-radius: var(--radius-lg);
  background: var(--fill-1);
}
.qr {
  flex: none;
  width: 104px;
  height: 104px;
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
  align-items: flex-start;
  gap: 6px;
  min-width: 0;
}
.url {
  display: flex;
  align-items: center;
  gap: 4px;
  width: 100%;
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
}
.status {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--text-sm);
  font-weight: 560;
  color: var(--text-2);
}
.status i {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text-3);
}
.status.live {
  color: var(--text-1);
}
.status.live i {
  background: var(--ok);
  box-shadow: 0 0 0 3px var(--ok-soft);
}
.fine {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.paste {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-3);
}
.paste .input {
  flex: 1;
  min-width: 0;
  font-size: var(--text-sm);
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
  transform: translateY(calc(-8px * var(--motion-scale)));
}
.stack-leave-to {
  opacity: 0;
}
</style>
