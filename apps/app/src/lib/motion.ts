// Motion helpers. Everything here degrades to a short cross-fade when the
// user prefers reduced motion, and never animates layout properties.
import { motionReduced } from "./appearance";

const SPRING_SOFT =
  "linear(0, 0.0293, 0.0988, 0.1884, 0.2849, 0.3803, 0.47, 0.5514, 0.6235, 0.6864, 0.7403, 0.7861, 0.8246, 0.8568, 0.8835, 0.9055, 0.9236, 0.9383, 0.9504, 0.9601, 0.968, 0.9744, 0.9796, 0.9837, 0.987, 0.9897, 0.9918, 0.9935, 0.9948, 0.9959, 0.9968, 0.9974, 1)";
const supportsLinear = typeof CSS !== "undefined" && CSS.supports?.("transition-timing-function", "linear(0, 1)");
export const springSoft = supportsLinear ? SPRING_SOFT : "cubic-bezier(0.22, 1, 0.36, 1)";

/**
 * Shared-element flight: a snapshot of `from` travels to `to` and shrinks
 * into it (a file chip landing on a device). Pure transform/opacity on a
 * fixed-position clone, so layout never animates.
 */
export function fly(from: Element, to: Element, delay = 0): Promise<void> {
  const a = from.getBoundingClientRect();
  const b = to.getBoundingClientRect();
  if (motionReduced() || !a.width || !b.width) return Promise.resolve();
  const clone = from.cloneNode(true) as HTMLElement;
  Object.assign(clone.style, {
    position: "fixed",
    left: `${a.left}px`,
    top: `${a.top}px`,
    width: `${a.width}px`,
    height: `${a.height}px`,
    margin: "0",
    zIndex: "1000",
    pointerEvents: "none",
    transformOrigin: "center",
  });
  document.body.appendChild(clone);
  const dx = b.left + b.width / 2 - (a.left + a.width / 2);
  const dy = b.top + b.height / 2 - (a.top + a.height / 2);
  // A slight arc: the midpoint lifts, so the chip reads as "thrown".
  const lift = Math.min(80, Math.hypot(dx, dy) * 0.18);
  const animation = clone.animate(
    [
      { transform: "translate(0, 0) scale(1)", opacity: 1, offset: 0 },
      { transform: `translate(${dx * 0.55}px, ${dy * 0.55 - lift}px) scale(0.82)`, opacity: 1, offset: 0.55 },
      { transform: `translate(${dx}px, ${dy}px) scale(0.3)`, opacity: 0, offset: 1 },
    ],
    { duration: 560, delay, easing: "cubic-bezier(0.5, 0, 0.3, 1)", fill: "forwards" },
  );
  return animation.finished.then(
    () => clone.remove(),
    () => clone.remove(),
  );
}

/** A brief "received" pulse on an element (target tile acknowledging a drop). */
export function pulse(el: Element) {
  if (motionReduced()) return;
  el.animate(
    [{ transform: "scale(1)" }, { transform: "scale(1.045)" }, { transform: "scale(1)" }],
    { duration: 420, easing: springSoft },
  );
}

/** Runs a DOM update inside a View Transition when supported. */
export function viewTransition(update: () => void | Promise<void>) {
  const doc = document as Document & { startViewTransition?: (cb: () => void | Promise<void>) => unknown };
  if (!doc.startViewTransition || motionReduced()) {
    void update();
    return;
  }
  doc.startViewTransition(update);
}
