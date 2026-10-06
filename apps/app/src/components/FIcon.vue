<script setup lang="ts">
// Consistent icon sizing/weight across the app.
import type { Component } from "vue";
withDefaults(defineProps<{ icon: Component; size?: number; stroke?: number }>(), { size: 18, stroke: 1.75 });
</script>

<template>
  <component :is="icon" :size="size" :stroke-width="stroke" aria-hidden="true" class="f-icon" />
</template>

<style scoped>
.f-icon {
  flex: none;
}
</style>
