<script setup lang="ts">
defineProps<{ modelValue: boolean; label: string; disabled?: boolean }>();
defineEmits<{ "update:modelValue": [value: boolean] }>();
</script>

<template>
  <button
    type="button"
    role="switch"
    class="f-toggle"
    :aria-checked="modelValue"
    :aria-label="label"
    :disabled="disabled"
    @click="$emit('update:modelValue', !modelValue)"
  >
    <span class="thumb" />
  </button>
</template>

<style scoped>
.f-toggle {
  position: relative;
  flex: none;
  width: 44px;
  height: 26px;
  padding: 0;
  border: 0;
  border-radius: var(--radius-pill);
  background: var(--fill-2);
  box-shadow: inset 0 0 0 1px var(--hairline);
  transition: background-color var(--dur-control) var(--ease-out);
}
.f-toggle[aria-checked="true"] {
  background: var(--accent);
  box-shadow: none;
}
.thumb {
  position: absolute;
  top: 3px;
  left: 3px;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: #fff;
  box-shadow:
    0 1px 2px rgba(0, 0, 0, 0.18),
    0 2px 6px rgba(0, 0, 0, 0.08);
  transition: transform var(--spring-snappy-dur) var(--spring-snappy);
}
.f-toggle[aria-checked="true"] .thumb {
  transform: translateX(18px);
  /* On the accent track: white on black (light), black on white (dark). */
  background: var(--text-on-accent);
}
.f-toggle:disabled {
  opacity: 0.45;
}
/* The rules above set box-shadow, which would hide the global focus ring. */
.f-toggle:focus-visible,
.f-toggle[aria-checked="true"]:focus-visible {
  box-shadow: var(--focus-ring);
}
</style>
