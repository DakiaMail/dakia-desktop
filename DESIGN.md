# Dakia design brief

## Brand idea

Dakia is a calm, private home for email. Its identity should feel warm, deliberate, and trustworthy: personal correspondence rather than corporate infrastructure.

The primary icon combines three ideas:

- a **D-shaped envelope** for the product and name;
- a **wax seal** for privacy, authorship, and care;
- a **soft ember tile** that feels approachable rather than technical.

This is a distinctive concept and fits Dakia's local-first, privacy-minded positioning. The envelope reads clearly at large sizes; the seal rewards closer inspection. Fine folds, the seal texture, and the nested “D” lose clarity when reduced, so Dakia uses a simpler mark in the interface.

## Logo system

### Primary app icon

Use the sealed-envelope icon for app stores, installers, launchers, repository artwork, and other placements at **64 px or larger**. The editable source is [`apps/desktop/src-tauri/icons/icon.svg`](apps/desktop/src-tauri/icons/icon.svg).

Preserve the rounded-square silhouette, D-shaped envelope, and central seal as one composition. Do not recolor individual parts, stretch the artwork, add a container, or place text inside the icon.

### Small-size mark

Use the ember tile with the lowercase white **d** for navigation, favicons, and compact brand signatures below 64 px. It is intentionally simpler than the app icon and should not be replaced by a miniature seal.

The tile has a subtly asymmetric radius: rounder at the top-left, top-right, and bottom-left; tighter at the bottom-right. Pair it with the word **Dakia** when space allows. Set the wordmark in the product sans-serif at a strong semibold weight with slightly tight tracking; do not typeset it as all caps.

### Spacing

Keep clear space around either mark equal to at least **one quarter of its width**. In a mark-and-word signature, use a gap of roughly **one third of the mark width**. Avoid placing the mark over photography or visually busy backgrounds.

## Core palette

| Role | Color | Use |
| --- | --- | --- |
| Ember | `#D65A3A` | Small mark, primary actions, active states |
| Ember light | `#D76043` | App-icon gradient start |
| Ember dark | `#B7432B` | App-icon gradient end |
| Envelope | `#FFF8F1` | Warm paper surface inside the icon |
| Fold line | `#C39572` | Envelope construction details |
| Seal | `#8E2330` | Wax seal body |
| Seal dark | `#6E1721` | Seal monogram and depth |
| Pine | `#244D46` | Dark surfaces and a calm secondary accent |
| Ink | `#1C2423` | Primary text |
| Paper | `#F6F7F3` | Warm application background |
| Muted | `#66716D` | Secondary text |

Ember is the identifying accent, not a general background color. Use it sparingly so actions and brand moments remain easy to find. Pine balances its warmth and reinforces privacy; off-white paper tones keep the product softer than pure black and white.

## Typography and visual character

The current system uses **Avenir Next**, falling back to **Segoe UI Variable**, **Segoe UI**, and `sans-serif`. Favor compact headings, slightly tight letter spacing, sentence case, and comfortable body copy. The voice should be direct, reassuring, and human—never cute, alarmist, or full of security jargon.

Interfaces should use generous whitespace, quiet borders, warm neutral surfaces, and restrained shadows. Rounded geometry may echo the icon, but avoid making every element pill-shaped. Motion, when present, should be short and functional.

## Accessibility

- Treat the icon as decorative when adjacent text already says “Dakia”; otherwise provide the accessible name **Dakia**.
- Do not rely on ember alone to communicate selection, error, or status.
- Check text and controls against WCAG AA contrast; the lighter icon colors are decorative and are not approved text colors.
- Prefer the small-size mark whenever the primary icon's envelope or seal is no longer immediately legible.

## Source assets

- Primary vector: [`apps/desktop/src-tauri/icons/icon.svg`](apps/desktop/src-tauri/icons/icon.svg)
- Public raster: [`apps/desktop/public/icon.png`](apps/desktop/public/icon.png)
- Generated platform icons: `apps/desktop/src-tauri/icons/`
- Small-size mark implementation: `apps/desktop/src/styles.css` (`.brand-mark`)

When the logo changes, update the vector first, regenerate all platform sizes, and verify the result at 16, 32, 64, 128, and 512 px on both light and dark surfaces.
