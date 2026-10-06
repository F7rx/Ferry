// Text cut off with an ellipsis gets its full value as a tooltip, wherever it
// appears. Elements with their own title are left alone.
const AUTO = "autoTitle";

function truncated(el: HTMLElement): boolean {
  return getComputedStyle(el).textOverflow === "ellipsis" && el.scrollWidth > el.clientWidth;
}

export function initTruncationTitles() {
  document.addEventListener(
    "pointerover",
    (event) => {
      let el = event.target instanceof HTMLElement ? event.target : null;
      for (let depth = 0; el && depth < 3; depth++, el = el.parentElement) {
        if (el.hasAttribute("title") && el.dataset[AUTO] === undefined) return;
        if (truncated(el)) {
          el.title = el.textContent?.trim() ?? "";
          el.dataset[AUTO] = "";
          return;
        }
        if (el.dataset[AUTO] !== undefined) {
          el.removeAttribute("title");
          delete el.dataset[AUTO];
        }
      }
    },
    { passive: true },
  );
}
