<script setup lang="ts">
import type { Component } from "vue";
import FIcon from "./FIcon.vue";

withDefaults(
  defineProps<{
    variant?: "primary" | "secondary" | "ghost" | "danger";
    size?: "sm" | "md" | "lg";
    icon?: Component;
    iconOnly?: boolean;
    label?: string;
    disabled?: boolean;
    type?: "button" | "submit";
  }>(),
  { variant: "secondary", size: "md", type: "button" },
);
</script>

<template>
  <button
    :type="type"
    class="f-button"
    :class="[variant, size, { 'icon-only': iconOnly }]"
    :disabled="disabled"
    :aria-label="iconOnly ? label : undefined"
    :title="iconOnly ? label : undefined"
  >
    <FIcon v-if="icon" :icon="icon" :size="size === 'sm' ? 16 : 18" />
    <span v-if="!iconOnly"><slot>{{ label }}</slot></span>
  </button>
</template>

<style scoped>
.f-button {
  --h: 40px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: var(--h);
  min-width: var(--h);
  padding: 0 16px;
  border-radius: var(--radius-pill);
  border: 1px solid transparent;
  font-size: var(--text-md);
  font-weight: 560;
  letter-spacing: -0.005em;
  white-space: nowrap;
  user-select: none;
  transition:
    background-color var(--dur-control) var(--ease-out),
    border-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out),
    box-shadow var(--dur-control) var(--ease-out),
    transform var(--dur-instant) var(--ease-out);
}
.f-button:active:not(:disabled) {
  transform: scale(calc(1 - 0.04 * var(--motion-scale)));
}
.f-button:disabled {
  opacity: 0.45;
}
.sm {
  --h: 32px;
  padding: 0 12px;
  font-size: var(--text-sm);
}
.lg {
  --h: 48px;
  padding: 0 22px;
  font-size: var(--text-base);
}
.icon-only {
  padding: 0;
  width: var(--h);
}
.primary {
  color: var(--text-on-accent);
  background: var(--accent-gradient);
  box-shadow:
    0 1px 1px rgba(0, 0, 0, 0.12),
    0 6px 16px -6px var(--accent-glow),
    inset 0 1px 0 rgba(255, 255, 255, 0.16);
}
@media (hover: hover) {
  .primary:hover:not(:disabled) {
    box-shadow:
      0 1px 1px rgba(0, 0, 0, 0.12),
      0 10px 22px -8px var(--accent-glow),
      inset 0 1px 0 rgba(255, 255, 255, 0.22);
  }
}
.secondary {
  color: var(--text-1);
  background: var(--fill-1);
  border-color: var(--hairline);
}
@media (hover: hover) {
  .secondary:hover:not(:disabled) {
    background: var(--fill-2);
  }
}
.ghost {
  color: var(--text-2);
  background: transparent;
}
@media (hover: hover) {
  .ghost:hover:not(:disabled) {
    color: var(--text-1);
    background: var(--fill-hover);
  }
}
.danger {
  color: var(--danger);
  background: var(--danger-soft);
}
@media (hover: hover) {
  .danger:hover:not(:disabled) {
    background: color-mix(in srgb, var(--danger) 16%, transparent);
  }
}
</style>
