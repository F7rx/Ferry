<script setup lang="ts">
import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from "@lucide/vue";
import { dismissToast, store } from "../stores/engine";
import FIcon from "./FIcon.vue";
const icons = { info: Info, success: CircleCheck, warning: TriangleAlert, error: CircleAlert };
</script>

<template>
  <div class="toasts" role="region" aria-label="Notifications">
    <TransitionGroup name="toast">
      <div v-for="t in store.toasts" :key="t.id" class="toast glass-2" :class="t.level" :role="t.level === 'error' ? 'alert' : 'status'">
        <FIcon :icon="icons[t.level]" :size="18" class="icon" />
        <div class="text">
          <strong>{{ t.title }}</strong>
          <span v-if="t.body">{{ t.body }}</span>
        </div>
        <button v-if="t.action" type="button" class="action" @click="t.action.run(); dismissToast(t.id)">{{ t.action.label }}</button>
        <button type="button" class="close" aria-label="Dismiss" @click="dismissToast(t.id)"><FIcon :icon="X" :size="14" /></button>
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.toasts {
  position: fixed;
  left: 50%;
  bottom: 24px;
  z-index: 60;
  display: flex;
  flex-direction: column-reverse;
  gap: 8px;
  width: min(440px, calc(100vw - 32px));
  transform: translateX(-50%);
  pointer-events: none;
}
.toast {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 12px 10px 12px 14px;
  border-radius: var(--radius-lg);
  pointer-events: auto;
}
.icon {
  margin-top: 1px;
  color: var(--accent);
}
.success .icon {
  color: var(--ok);
}
.warning .icon {
  color: var(--warn);
}
.error .icon {
  color: var(--danger);
}
.text {
  flex: 1;
  display: flex;
  flex-direction: column;
  font-size: var(--text-sm);
  line-height: 1.4;
}
.text strong {
  font-weight: 600;
}
.text span {
  color: var(--text-2);
}
.action {
  border: 0;
  background: none;
  font-weight: 600;
  font-size: var(--text-sm);
  color: var(--accent-text);
}
.close {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border: 0;
  border-radius: 50%;
  background: none;
  color: var(--text-3);
}
@media (hover: hover) {
  .close:hover {
    background: var(--fill-hover);
  }
}
.toast-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-soft-dur) var(--spring-soft);
}
.toast-leave-active {
  transition:
    opacity var(--dur-small) var(--ease-in),
    transform var(--dur-small) var(--ease-in);
}
.toast-enter-from,
.toast-leave-to {
  opacity: 0;
  transform: translateY(calc(12px * var(--motion-scale))) scale(calc(1 - 0.03 * var(--motion-scale)));
}
@media (max-width: 639px) {
  .toasts {
    bottom: calc(84px + env(safe-area-inset-bottom));
  }
}
</style>
