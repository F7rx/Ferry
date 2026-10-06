<script setup lang="ts">
import { computed } from "vue";
import type { DeviceKind } from "../platform";
import { deviceIcon } from "../lib/icons";
import FIcon from "./FIcon.vue";
const props = withDefaults(
  defineProps<{ kind: DeviceKind; model?: string | null; size?: number; online?: boolean; tone?: "default" | "accent" }>(),
  { size: 44, online: true, tone: "default" },
);
const icon = computed(() => deviceIcon(props.kind, props.model));
</script>

<template>
  <span class="avatar" :class="[tone, { offline: !online }]" :style="{ width: `${size}px`, height: `${size}px` }">
    <FIcon :icon="icon" :size="Math.round(size * 0.46)" :stroke="1.6" />
  </span>
</template>

<style scoped>
.avatar {
  display: grid;
  place-items: center;
  flex: none;
  border-radius: 30%;
  color: var(--accent-strong);
  background: linear-gradient(180deg, var(--solid-1), var(--solid-2));
  box-shadow:
    inset 0 0 0 1px var(--hairline),
    0 1px 2px rgba(0, 0, 0, 0.06);
}
:root[data-theme="dark"] .avatar {
  color: var(--accent-text);
}
.accent {
  color: var(--text-on-accent);
  background: var(--accent-gradient);
  box-shadow: 0 6px 14px -6px var(--accent-glow);
}
.offline {
  color: var(--text-3);
  filter: saturate(0.4);
}
</style>
