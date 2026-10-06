<script setup lang="ts">
// The Ferry mark, drawn inline so it follows the theme: ink on light,
// light on dark (the accent and its foreground). public/icon.svg is the
// same drawing in black for app icons and favicons.
withDefaults(defineProps<{ size?: number }>(), { size: 30 });
</script>

<template>
  <svg class="mark" :width="size" :height="size" viewBox="0 0 512 512" aria-hidden="true">
    <rect x="24" y="24" width="464" height="464" rx="118" class="tile" />
    <!-- origin: an open ring; destination: a solid point; the arc is the hop between them -->
    <circle cx="154" cy="346" r="30" fill="none" class="ink" stroke-width="22" stroke-opacity="0.82" />
    <path d="M182 312 C 214 214, 292 166, 338 196" fill="none" class="ink" stroke-width="30" stroke-linecap="round" />
    <circle cx="366" cy="222" r="36" class="dot" />
  </svg>
</template>

<style scoped>
.mark {
  flex: none;
}
.tile {
  fill: var(--mark-tile);
}
.ink {
  stroke: var(--mark-ink);
}
.dot {
  fill: var(--mark-ink);
}
</style>
