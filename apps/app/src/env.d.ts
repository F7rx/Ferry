/// <reference types="vite/client" />
declare const __FERRY_TARGET__: "desktop" | "web";
declare const __FERRY_VERSION__: string;
declare module "*.vue" {
  import type { DefineComponent } from "vue";
  const component: DefineComponent<object, object, unknown>;
  export default component;
}
