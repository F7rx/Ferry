<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import NavRail from "./components/NavRail.vue";
import TabBar from "./components/TabBar.vue";
import ReceiveHandoff from "./components/ReceiveHandoff.vue";
import Toasts from "./components/Toasts.vue";
import { native, platform, web, type OutgoingItem } from "./platform";
import { compose, sendNow, stage } from "./stores/compose";
import { itemsFromDataTransfer } from "./lib/dropfiles";
import { activeTransfers, initEngine, store } from "./stores/engine";
import FButton from "./components/FButton.vue";
import { pulse } from "./lib/motion";

const router = useRouter();
const route = useRoute();
const width = ref(window.innerWidth);
const onResize = () => (width.value = window.innerWidth);
const layout = computed(() => (width.value < 640 ? "mobile" : width.value < 1024 ? "tablet" : "desktop"));

function deviceAt(x: number, y: number): string | null {
  const el = document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-device-id]");
  return el?.dataset.deviceId ?? null;
}

async function dropped(items: OutgoingItem[], x: number, y: number) {
  const target = compose.hoverTarget ?? deviceAt(x, y);
  compose.dragging = false;
  compose.hoverTarget = null;
  if (!items.length) return;
  if (target && compose.items.length === 0) {
    // Quick Drop: straight to the device under the cursor.
    const tile = document.querySelector(`[data-device-id="${CSS.escape(target)}"]`);
    if (tile) pulse(tile);
    await sendNow(target, items);
    return;
  }
  stage(items);
  if (target) compose.targets.add(target);
  if (route.path !== "/" && route.path !== "/send") router.push("/");
}

// Native drag-and-drop gives real paths plus the cursor position.
let unlistenDrag: (() => void) | undefined;
let unlistenPaths: (() => void) | undefined;
// Browser drag-and-drop (PWA / preview).
let dragDepth = 0;
function onDragEnter(e: DragEvent) {
  if (!e.dataTransfer?.types.includes("Files")) return;
  dragDepth++;
  compose.dragging = true;
}
function onDragOver(e: DragEvent) {
  if (!compose.dragging) return;
  e.preventDefault();
  compose.hoverTarget = deviceAt(e.clientX, e.clientY);
}
function onDragLeave() {
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) {
    compose.dragging = false;
    compose.hoverTarget = null;
  }
}
function onDrop(e: DragEvent) {
  if (!e.dataTransfer?.files.length) return;
  e.preventDefault();
  dragDepth = 0;
  const { clientX, clientY } = e;
  // Folders are walked recursively; the entries must be taken during the event.
  void itemsFromDataTransfer(e.dataTransfer).then((items) => dropped(items, clientX, clientY));
}
async function onPaste(e: ClipboardEvent) {
  const target = e.target as HTMLElement;
  if (target.closest("input, textarea, [contenteditable]")) return;
  const files = [...(e.clipboardData?.files ?? [])];
  if (files.length) {
    stage(files.map((file) => ({ kind: "file", file, name: file.name, size: file.size })));
    return;
  }
  const text = e.clipboardData?.getData("text/plain");
  if (text?.trim()) stage([{ kind: "text", text: text.trim(), name: "Text" }]);
}

// In the browser a transfer lives in this tab: confirm before closing it.
function onBeforeUnload(e: BeforeUnloadEvent) {
  if (!web || !activeTransfers.value.length) return;
  e.preventDefault();
  e.returnValue = "";
}

onMounted(() => {
  window.addEventListener("beforeunload", onBeforeUnload);
  window.addEventListener("resize", onResize);
  window.addEventListener("paste", onPaste);
  const n = native;
  if (n) {
    unlistenDrag = n.onDrag(async (e) => {
      if (e.type === "enter") {
        compose.dragging = true;
        compose.hoverTarget = deviceAt(e.x, e.y);
      } else if (e.type === "over") {
        compose.hoverTarget = deviceAt(e.x, e.y);
      } else if (e.type === "leave") {
        compose.dragging = false;
        compose.hoverTarget = null;
      } else if (e.type === "drop") {
        const items = await n.pathItems(e.paths);
        await dropped(items, e.x, e.y);
      }
    });
    unlistenPaths = n.onPaths((items) => {
      stage(items);
      router.push("/");
    });
  } else {
    window.addEventListener("dragenter", onDragEnter);
    window.addEventListener("dragover", onDragOver);
    window.addEventListener("dragleave", onDragLeave);
    window.addEventListener("drop", onDrop);
  }
});
onBeforeUnmount(() => {
  window.removeEventListener("beforeunload", onBeforeUnload);
  window.removeEventListener("resize", onResize);
  window.removeEventListener("paste", onPaste);
  unlistenDrag?.();
  unlistenPaths?.();
  window.removeEventListener("dragenter", onDragEnter);
  window.removeEventListener("dragover", onDragOver);
  window.removeEventListener("dragleave", onDragLeave);
  window.removeEventListener("drop", onDrop);
});
const isDemo = platform.capabilities.kind === "demo";
</script>

<template>
  <div class="app" :class="layout">
    <a href="#main" class="skip">Skip to content</a>
    <NavRail v-if="layout !== 'mobile'" :collapsed="layout === 'tablet'" />
    <main id="main" class="content scroll" tabindex="-1">
      <div v-if="!store.ready" class="loading" role="status">
        <div v-if="store.syncError" class="load-failed">
          <span>Ferry couldn't start: {{ store.syncError }}</span>
          <FButton variant="primary" @click="initEngine()">Retry</FButton>
        </div>
        <template v-else>Starting Ferry…</template>
      </div>
      <RouterView v-else v-slot="{ Component, route: r }">
        <Transition name="page" mode="out-in">
          <component :is="Component" :key="r.path" />
        </Transition>
      </RouterView>
    </main>
    <TabBar v-if="layout === 'mobile'" />
    <ReceiveHandoff />
    <Toasts />
    <aside v-if="isDemo && layout === 'mobile'" class="demo-ribbon" aria-label="Preview mode">Preview with simulated devices</aside>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  gap: 16px;
  height: 100dvh;
  padding: 16px;
}
.app.mobile {
  padding: 0;
}
.content {
  flex: 1;
  min-width: 0;
  height: 100%;
  border-radius: var(--radius-xl);
}
.content:focus {
  outline: none;
}
.mobile .content {
  border-radius: 0;
  padding-bottom: calc(96px + env(safe-area-inset-bottom));
}
.loading {
  display: grid;
  place-items: center;
  height: 100%;
  color: var(--text-3);
}
.load-failed {
  display: grid;
  justify-items: center;
  gap: 12px;
  max-width: 420px;
  padding: 0 16px;
  text-align: center;
}
.skip {
  position: fixed;
  top: -100px;
  left: 16px;
  z-index: 100;
  padding: 8px 14px;
  border-radius: var(--radius-pill);
  background: var(--solid-1);
  box-shadow: var(--shadow-2);
}
.skip:focus {
  top: 16px;
}
.demo-ribbon {
  position: fixed;
  top: 0;
  left: 50%;
  z-index: 70;
  padding: 3px 10px;
  border-radius: 0 0 8px 8px;
  background: var(--warn-soft);
  color: var(--warn);
  font-size: var(--text-2xs);
  font-weight: 600;
  transform: translateX(-50%);
}
.page-enter-active,
.page-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-out),
    transform var(--dur-small) var(--ease-out);
}
.page-enter-from {
  opacity: 0;
  transform: translateY(calc(6px * var(--motion-scale)));
}
.page-leave-to {
  opacity: 0;
}
</style>
