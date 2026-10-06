<script setup lang="ts" generic="T extends string">
defineProps<{ modelValue: T; options: { value: T; label: string }[]; label: string }>();
defineEmits<{ "update:modelValue": [value: T] }>();
</script>

<template>
  <div class="f-segmented" role="radiogroup" :aria-label="label">
    <button
      v-for="o in options"
      :key="o.value"
      type="button"
      role="radio"
      :aria-checked="modelValue === o.value"
      :class="{ on: modelValue === o.value }"
      @click="$emit('update:modelValue', o.value)"
    >
      {{ o.label }}
    </button>
  </div>
</template>

<style scoped>
.f-segmented {
  display: inline-flex;
  flex: none;
  max-width: 100%;
  overflow-x: auto;
  scrollbar-width: none;
  padding: 3px;
  gap: 2px;
  border-radius: var(--radius-pill);
  background: var(--fill-1);
  box-shadow: inset 0 0 0 1px var(--hairline);
}
button {
  flex: none;
  white-space: nowrap;
  height: 32px;
  padding: 0 14px;
  border: 0;
  border-radius: var(--radius-pill);
  background: transparent;
  color: var(--text-2);
  font-size: var(--text-sm);
  font-weight: 540;
  transition:
    background-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out),
    box-shadow var(--dur-control) var(--ease-out);
}
@media (hover: hover) {
  button:hover {
    color: var(--text-1);
  }
}
button.on {
  background: var(--solid-1);
  color: var(--text-1);
  box-shadow: var(--shadow-1);
}
</style>
