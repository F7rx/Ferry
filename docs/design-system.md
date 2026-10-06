# Ferry design system

One stylesheet (`apps/app/src/styles/tokens.css` + `base.css`) drives the
desktop app, the mobile app and the PWA; they render the same Vue components.

## Principles

1. **Glass is hierarchy, not decoration.** Exactly two material levels; content inside a module uses tinted wells, never a second blur.
2. **Calm by default, alive on interaction.** Surfaces are quiet until something happens: a drag, a request, a transfer.
3. **Every motion has a job:** show where something came from or went, confirm an action, or reveal state. Nothing loops for attention.
4. **Accessibility wins ties.** Contrast, focus, target size and reduced motion/transparency override the visual reference.

## Materials

| Level | Class | Used for | Light | Dark |
|---|---|---|---|---|
| 0 | `body` | Page background | `#f3f3f3` + two soft neutral light pools | `#000000` + faint grey pools |
| 1 | `.glass` | Navigation rail, workspace modules | white 72 %, blur 22 px | `rgb(22 22 22 / 72 %)` with an 8 % white rim |
| 2 | `.glass-2` | Floating overlays: receive handoff, toasts, device tiles, tab bar | white 86 %, blur 34 px, deeper shadow | `rgb(28 28 28 / 90 %)` |
| n/a | `.fill-*` / wells | Inside modules (chips, facts, inputs) | ink at 3.5 to 7.5 % | white at 3.5 to 8 % |

Every glass surface has a 1 px hairline rim plus a 1 px top highlight
(`inset 0 1px 0 var(--glass-edge)`): light catching the edge.

**Reduced transparency** (`data-transparency="reduced"`, from Settings or the
OS preference) swaps glass for solid surfaces and removes all blur. Chosen
automatically where `prefers-reduced-transparency: reduce` is reported.

## Color

- **Neutral surfaces, colour with a job.** Light theme: white surfaces and black text; dark theme: black surfaces and white text. Colour appears only for accents and for things that need attention:
  - Accent blue `#3366ff` (white labels 4.7:1): primary buttons, toggles, selection, progress, badges, focus ring. Link text `#2350e6` on light (6.2:1), `#8fb0ff` on dark (8.9:1).
  - Status: ok green (`#0e8546` / `#3ccf82`), warn amber (`#a35f00` / `#f0b04a`), danger red for errors and destructive actions only (`#c8102e` 5.9:1 / `#ff5c6a` 6.2:1). A decline or a cancel isn't red: it's an outcome, not a fault. Icons and words always carry the meaning too.
- Text: `--text-1/2/3` at 19.8 / 11.0 / 5.7 : 1 on white (5.2:1 for `--text-3` on the bare background); dark `#f5f5f5 / #b8b8b8 / #8f8f8f`, `--text-3` at 5.8:1 on surfaces.
- QR codes always sit on a white tile, in both themes, because scanners need dark-on-light.
- The logo is drawn inline (`FerryMark.vue`) so it follows the theme; `public/icon.svg` and the generated app/PWA icons are the black version (`scripts/icons.mjs`, `npx tauri icon public/icon.svg`).
- Contrast ratios are computed from the token values, not eyeballed; `prefers-contrast: more` strengthens hairlines and secondary text.

## Type

Geist Variable (OFL), Geist Mono for codes and addresses. Tabular numerals
(`.tabular`) for every changing number (speed, size, ETA).

| Token | Size | Use |
|---|---|---|
| `--text-display` | 36 → 54 px fluid, −0.035 em, weight 700 | "Drop anything." |
| `--text-2xl` | 28 px, −0.02 em | Page titles |
| `--text-lg` / `--text-base` | 17 / 15 px | Module titles, body |
| `--text-md` / `--text-sm` | 14 / 13 px | Controls, secondary |
| `--text-xs` / `--text-2xs` | 12 / 11 px | Metadata, badges |

## Space, radius, targets

4 px grid (`--space-1` … `--space-16`). Concentric radii: module 28, tile 22,
chip 16, inner 12, control pill. Minimum hit target 44 px (`--hit`); mobile
tab targets 52 px.

## Motion

| Token | Duration | Curve | For |
|---|---|---|---|
| `--dur-instant` | 100 ms | ease-out | press feedback |
| `--dur-control` | 150 ms | ease-out | hover, toggles, chips |
| `--dur-small` | 220 ms | ease-out | small state changes |
| `--dur-panel` | 320 ms | ease-out | panels, sheets, cards |
| `--spring-snappy` | 380 ms | ζ 1.0 spring | toggles, checks |
| `--spring-soft` | 510 ms | ζ 1.0 spring | tiles settling, handoff entering |
| `--spring-settle` | 540 ms | ζ 0.82 spring (≈1 % overshoot) | only for things that were thrown/dropped (file chips landing) |

Springs are sampled into CSS `linear()` curves (no JS animation runtime).
Positions animate with `transform`/`translate`/`scale` only; layout never
animates. Shared-element flights (a file chip travelling to the device it is
sent to) use WAAPI on a fixed-position clone.

Choreography:

| Moment | Motion |
|---|---|
| Device appears | soft spring from 92 % scale + fade, staggered 60 ms |
| Files dragged over window | well grows 7 % with accent ring; tiles get dashed accent rims |
| Drag over a device | that tile lifts 4 px, scales 5 %, accent ring; dropping sends immediately (Quick Drop) |
| Files staged | chips materialize (blur → sharp, small rise, settle spring) |
| Send | chips arc toward each chosen device and dissolve into it; the tile pulses |
| Incoming request | handoff card arrives from the top-right (bottom on phones) and leaves the same way |
| Progress | ring stroke flows linearly between updates; check icon morphs in on completion |

**Reduced motion** (`data-motion="reduced"`): `--motion-scale` becomes 0, so
every translate/scale distance collapses; springs become 160 to 200 ms ease-out
fades; looping pulses stop. Feedback (color, opacity, state) remains.

## Layout

| Width | Layout |
|---|---|
| ≥ 1100 px | Glass nav rail (232 px) + workspace: hero 8 fr, side column 4 fr |
| 1024 to 1099 px | Rail + single column |
| 640 to 1023 px | Icon rail (72 px) + single column |
| < 640 px | Bottom tab bar (Nearby, Transfers, Inbox, Settings), single column, large device tiles in a grid |

The drop stage places nearby devices around the well on an ellipse with a
greedy, collision-free placement (sides first, then top/bottom); what doesn't
fit rolls into "+N more". Below 640 px it becomes a grid of touch tiles.

## Components

`FButton` (primary/secondary/ghost/danger; sm/md/lg; icon-only with label) ·
`FToggle` (switch role) · `FSegmented` (radiogroup) · `ProgressRing` ·
`DeviceAvatar` · `DeviceTile` · `FileChip` · `DropStage` · `TransferCard` ·
`ReceiveHandoff` · `Toasts` · `NavRail` · `TabBar`.

## Accessibility checklist

- All interactive elements are real buttons/links with accessible names; icon-only buttons carry `aria-label` + tooltip.
- Visible focus ring (`--focus-ring`) on every focusable element; skip link to content.
- Incoming request: Enter accepts, Escape declines; the card is `role="dialog"` (non-modal) and announced politely.
- Status changes (`aria-live`) for transfer status and toasts; errors use `role="alert"`.
- No information by color alone: encryption, trust and errors always have an icon and text.
