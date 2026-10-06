<script setup lang="ts">
import { computed } from "vue";
const props = withDefaults(
  defineProps<{ value: number; size?: number; stroke?: number; indeterminate?: boolean; tone?: "accent" | "ok" | "warn" | "danger" | "muted" }>(),
  { size: 44, stroke: 3, tone: "accent" },
);
const r = computed(() => (props.size - props.stroke) / 2);
const c = computed(() => 2 * Math.PI * r.value);
const offset = computed(() => c.value * (1 - Math.min(1, Math.max(0, props.value))));
</script>

<template>
  <div class="ring" :class="[tone, { indeterminate }]" :style="{ width: `${size}px`, height: `${size}px` }">
    <svg :width="size" :height="size" :viewBox="`0 0 ${size} ${size}`" aria-hidden="true">
      <circle class="track" :cx="size / 2" :cy="size / 2" :r="r" :stroke-width="stroke" />
      <circle
        class="bar"
        :cx="size / 2"
        :cy="size / 2"
        :r="r"
        :stroke-width="stroke"
        :stroke-dasharray="c"
        :stroke-dashoffset="indeterminate ? c * 0.72 : offset"
      />
    </svg>
    <div class="inner"><slot /></div>
  </div>
</template>

<style scoped>
.ring {
  position: relative;
  flex: none;
  display: grid;
  place-items: center;
}
svg {
  position: absolute;
  inset: 0;
  transform: rotate(-90deg);
}
circle {
  fill: none;
}
.track {
  stroke: var(--fill-2);
}
.bar {
  stroke: var(--accent);
  stroke-linecap: round;
  /* The ring flows between progress updates instead of jumping. */
  transition:
    stroke-dashoffset 260ms linear,
    stroke var(--dur-small) var(--ease-out);
}
.ok .bar {
  stroke: var(--ok);
}
.warn .bar {
  stroke: var(--warn);
}
.danger .bar {
  stroke: var(--danger);
}
.muted .bar {
  stroke: var(--text-3);
}
.indeterminate svg {
  animation: spin 1.1s linear infinite;
}
:root[data-motion="reduced"] .indeterminate svg {
  animation: none;
}
@keyframes spin {
  to {
    transform: rotate(270deg);
  }
}
.inner {
  position: relative;
  display: grid;
  place-items: center;
  color: var(--text-2);
}
</style>
