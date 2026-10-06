<script setup lang="ts">
import { computed } from "vue";
import { Check, LockOpen, ShieldCheck, Star } from "@lucide/vue";
import type { DeviceSummary } from "../platform";
import { displayName } from "../stores/engine";
import DeviceAvatar from "./DeviceAvatar.vue";
import FIcon from "./FIcon.vue";

const props = defineProps<{
  device: DeviceSummary;
  /** Files are staged: the tile toggles selection instead of sending. */
  selectable?: boolean;
  selected?: boolean;
  /** Files are being dragged over the window. */
  receptive?: boolean;
  /** The cursor is over this tile during a drag (Quick Drop). */
  targeted?: boolean;
  compact?: boolean;
}>();
defineEmits<{ activate: [] }>();

const status = computed(() => {
  const d = props.device;
  if (!d.online) return "Offline";
  if (!d.verified) return "Not encrypted";
  if (d.mine) return "My device";
  if (d.trusted) return "Trusted";
  return d.isFerry ? "Ferry" : "LocalSend";
});
const signal = computed(() => {
  const rtt = props.device.rttMs;
  if (!props.device.online || rtt == null) return 0;
  return rtt < 15 ? 3 : rtt < 60 ? 2 : 1;
});
const label = computed(() => {
  const d = props.device;
  const action = props.selectable ? (props.selected ? "Deselect" : "Select") : "Send to";
  return `${action} ${displayName(d)}, ${d.deviceModel ?? d.deviceKind}, ${status.value}`;
});
</script>

<template>
  <button
    type="button"
    class="tile"
    :class="{ selected, receptive, targeted, compact, offline: !device.online }"
    :aria-label="label"
    :aria-pressed="selectable ? selected : undefined"
    :data-device-id="device.id"
    @click="$emit('activate')"
  >
    <span class="avatar-wrap">
      <DeviceAvatar :kind="device.deviceKind" :model="device.deviceModel" :size="compact ? 38 : 46" :online="device.online" :tone="selected || targeted ? 'accent' : 'default'" />
      <span v-if="selectable" class="check" :class="{ on: selected }" aria-hidden="true">
        <FIcon :icon="Check" :size="12" :stroke="3" />
      </span>
    </span>
    <span class="name">{{ displayName(device) }}</span>
    <span class="meta">
      <FIcon v-if="device.mine || device.trusted" :icon="ShieldCheck" :size="13" class="trust" />
      <FIcon v-else-if="!device.verified" :icon="LockOpen" :size="13" class="warn" />
      <span>{{ status }}</span>
      <span v-if="signal" class="signal" :aria-label="`Signal ${signal} of 3`">
        <i v-for="n in 3" :key="n" :class="{ on: n <= signal }" />
      </span>
    </span>
    <FIcon v-if="device.favorite" :icon="Star" :size="13" class="fav" />
  </button>
</template>

<style scoped>
.tile {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  width: 132px;
  padding: 14px 10px 12px;
  border-radius: var(--radius-lg);
  border: 1px solid var(--glass-rim);
  background: var(--glass-2);
  -webkit-backdrop-filter: blur(16px) saturate(var(--glass-saturate));
  backdrop-filter: blur(16px) saturate(var(--glass-saturate));
  box-shadow: inset 0 1px 0 var(--glass-edge), var(--shadow-2);
  color: var(--text-1);
  text-align: center;
  transition:
    transform var(--spring-soft-dur) var(--spring-soft),
    box-shadow var(--dur-small) var(--ease-out),
    border-color var(--dur-small) var(--ease-out),
    background-color var(--dur-small) var(--ease-out);
}
.tile.compact {
  width: 116px;
  padding: 12px 8px 10px;
}
@media (hover: hover) {
  .tile:hover {
    transform: translateY(calc(-3px * var(--motion-scale)));
    box-shadow: inset 0 1px 0 var(--glass-edge), var(--shadow-lift);
  }
}
.tile:active {
  transform: translateY(0) scale(calc(1 - 0.02 * var(--motion-scale)));
  transition-duration: var(--dur-instant);
}
.tile.selected {
  border-color: color-mix(in srgb, var(--accent) 55%, transparent);
  box-shadow:
    inset 0 1px 0 var(--glass-edge),
    0 0 0 3px var(--accent-soft),
    var(--shadow-lift);
}
.tile.receptive {
  border-color: color-mix(in srgb, var(--accent) 35%, transparent);
  border-style: dashed;
}
.tile.targeted {
  border-style: solid;
  border-color: var(--accent);
  transform: translateY(calc(-4px * var(--motion-scale))) scale(calc(1 + 0.05 * var(--motion-scale)));
  box-shadow:
    inset 0 1px 0 var(--glass-edge),
    0 0 0 4px var(--accent-soft),
    0 18px 40px -14px var(--accent-glow);
}
.tile.offline {
  opacity: 0.72;
}
:root[data-transparency="reduced"] .tile {
  -webkit-backdrop-filter: none;
  backdrop-filter: none;
}
.avatar-wrap {
  position: relative;
}
.check {
  position: absolute;
  right: -5px;
  bottom: -5px;
  display: grid;
  place-items: center;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  color: transparent;
  background: var(--solid-1);
  box-shadow: inset 0 0 0 1.5px var(--hairline-strong);
  transition:
    background-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out),
    transform var(--spring-snappy-dur) var(--spring-snappy);
}
.check.on {
  color: var(--text-on-accent);
  background: var(--accent);
  box-shadow: 0 0 0 2px var(--solid-1);
  transform: scale(calc(1 + 0.08 * var(--motion-scale)));
}
.name {
  max-width: 100%;
  overflow: hidden;
  font-size: var(--text-sm);
  font-weight: 600;
  letter-spacing: -0.01em;
  line-height: 1.25;
  white-space: nowrap;
  text-overflow: ellipsis;
}
/* Touch screens have no tooltip to reveal a cut-off name: give it two lines. */
@media (max-width: 639px) {
  .name {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    white-space: normal;
    overflow-wrap: anywhere;
    text-wrap: balance;
  }
}
.meta {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  max-width: 100%;
  font-size: var(--text-2xs);
  color: var(--text-3);
  white-space: nowrap;
}
.trust {
  color: var(--ok);
}
.warn {
  color: var(--warn);
}
.fav {
  position: absolute;
  top: 9px;
  right: 9px;
  color: var(--accent);
  fill: currentColor;
}
.signal {
  display: inline-flex;
  align-items: flex-end;
  gap: 1.5px;
  height: 9px;
  margin-left: 2px;
}
.signal i {
  width: 2.5px;
  border-radius: 1px;
  background: var(--hairline-strong);
}
.signal i:nth-child(1) {
  height: 4px;
}
.signal i:nth-child(2) {
  height: 6.5px;
}
.signal i:nth-child(3) {
  height: 9px;
}
.signal i.on {
  background: var(--ok);
}
</style>
