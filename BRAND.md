# Qpedia — Brand & Asset Spec

The single source of truth for the logo, colors, and icon assets. Pulled
from the live app theme (`web/src/app.css`) so the mark and the UI accent
stay in lockstep.

> **Name:** Qpedia · **Publisher:** Qern · **Domain:** qern.net
> A *quern* is a hand-mill that grinds grain into flour — Qpedia grinds raw
> documents into refined, linked knowledge. The metaphor backs the brand
> without anyone needing to get the pun.

---

## 1. Logo

**Concept B — the "Q" lettermark.** A circular ring (the Q) of even,
medium-bold stroke weight, with a single tail that crosses the ring on a
45° lower-right diagonal and terminates in a small four-point spark. The
tail + spark are **one continuous element** (this is what keeps it
balanced — two separate shapes fight each other). The spark's width equals
the ring's stroke width. Nothing else in the mark.

**Generation prompt** (for an image LLM; raster output is a *concept* —
redraw as vector for production):

```
A minimal flat-vector logo: a single geometric letter "Q" as a clean
emblem. The Q is a near-perfect circular ring of even, medium-bold stroke
weight with generous, even negative space inside. The tail is the ONLY
accent: a single short straight stroke crossing the ring at the lower-right
on a 45° diagonal and terminating in a small four-point spark. The tail and
spark are ONE continuous element, not two separate shapes — the spark's
width equals the ring's stroke width and it sits just outside the ring on
that diagonal. Two colors only: sky-blue (#38bdf8) on deep slate-navy
(#0f172a). Flat, crisp, high contrast, no gradients/3D/shadows/text. Square
1:1, optically centered, mark fills ~70% of the frame. Legible at 16px.
Linear/Vercel restraint.
```

**Construction rules**
- **Clear space:** ≥ 1× the ring stroke width of padding on all sides. On a
  tile, the mark fills **64–72%** of the square.
- **Minimum size:** 16px for the bare mark. Don't pair it with a wordmark
  below ~80px wide.
- **Optical centering:** the tail pulls visual weight to the lower-right;
  nudge the ring up-left so the *whole* mark looks centered, not the ring.

---

## 2. Color palette

Design tokens, straight from the app theme. (Hex = the value; the Tailwind
name is given because the palette is Tailwind **slate + sky** — handy for
designers.)

| Token | Hex | Tailwind | Role |
|---|---|---|---|
| `--bg` | `#0f172a` | slate-900 | App background; **logo tile** |
| `--bg-2` | `#1e293b` | slate-800 | Cards, panels |
| `--bg-3` | `#334155` | slate-700 | Hover / raised surfaces |
| `--border` | `#334155` | slate-700 | Hairlines |
| `--code-bg` | `#0b1220` | (custom) | Code blocks (near-black navy) |
| `--fg` | `#e2e8f0` | slate-200 | Primary text |
| `--fg-dim` | `#94a3b8` | slate-400 | Muted text |
| **`--accent`** | **`#38bdf8`** | **sky-400** | **Brand / logo / primary action** |
| `--accent-hover` | `#7dd3fc` | sky-300 | Hover accent |
| `--ok` | `#4ade80` | green-400 | Success |
| `--warn` | `#fbbf24` | amber-400 | Warning |
| `--err` | `#f87171` | red-400 | Error |

**The brand color is sky-400 `#38bdf8`.** The logo is two-color: `#38bdf8`
mark on `#0f172a` tile.

### Logo lockups
| Lockup | Mark | Background | Use |
|---|---|---|---|
| **Primary** | `#38bdf8` | `#0f172a` rounded tile | App icon, OAuth consent, favicon tile |
| Reverse | `#38bdf8` | transparent | On the dark in-app UI |
| Mono-dark | `#0f172a` | white | Light docs, invoices, print |
| Knockout | white | `#38bdf8` | Rare — spot accents only |

> **Contrast watch:** OAuth consent screens are usually **white**, and bare
> `#38bdf8` on white is borderline at small sizes. So the **app icon must be
> the Primary lockup** (sky mark on a dark slate tile) — it pops on both
> light and dark surroundings. Reserve the bare/transparent mark for the
> already-dark Qpedia UI.

---

## 3. Favicon & app-icon asset set

Shipped in `web/static/` (SvelteKit copies it to the build root; the
`<head>` suite in `app.html` + `site.webmanifest` reference them). Square,
centered, Primary lockup (dark tile) unless noted.

| File | Size | Format | Where it's used |
|---|---|---|---|
| `favicon.svg` | vector | SVG | Modern browser tabs (primary) |
| `favicon-32.png` | 32×32 | PNG | Fallback tab icon |
| `apple-touch-icon.png` | 180×180 | PNG (no alpha) | iOS home screen |
| `icon-192.png` | 192×192 | PNG | PWA manifest |
| `icon-512.png` | 512×512 | PNG | PWA manifest, splash, **OAuth icon source** |
| `maskable-512.png` | 512×512 | PNG | PWA maskable — mark inside the central safe circle, tile bleeds to edges |
| `site.webmanifest` | — | JSON | PWA manifest |

Notes:
- **OAuth registrations** downscale `icon-512.png` to each provider's size
  (Google ~120, Entra ~215, Slack 512) — same Primary lockup everywhere.
- **`favicon.ico`** (legacy multi-res) is optional; the SVG + 32px PNG cover
  every current browser. Add it only for IE / old-bookmark support.
- **Tiny sizes** (≤24px): if the spark muddies, ship a ring-only "Q"
  variant — the ring alone still reads as the mark.
- **`apple-touch-icon`**: no transparency (iOS adds its own corners); solid
  dark tile edge-to-edge. ✓ as produced.

---

## 4. OAuth app registration

The logo + name front **every** connector consent screen (Google now;
Microsoft, GitHub, Slack per the roadmap). Register **identical** name and
icon everywhere so a user connecting their third source recognizes the same
Qpedia they trusted on the first.

| Field | Value |
|---|---|
| App / display name | **Qpedia** |
| Publisher | **Qern** |
| Verified domain | **qern.net** |
| Homepage / support | `qern.net` (or `qpedia.qern.net`) |
| Icon | Primary lockup (sky mark, dark tile), square |

**Per-provider icon (confirm current limits at registration):**
| Provider | Icon | Notes |
|---|---|---|
| Google | ~120×120 PNG, square | `drive.readonly` is a **sensitive scope** → app verification required, or the consent screen shows "unverified app" + a 100-user cap. **Finalize the name + logo before submitting for verification** (renaming = re-verify). |
| Microsoft (Entra) | ~215×215 PNG, ≤100 KB | App registration → Branding |
| GitHub | ≥200×200, square | OAuth App → Application logo |
| Slack | 512×512 PNG, ≤2 MB | Shown rounded |

---

## 5. Usage do / don't

**Do**
- Use the Primary lockup on any external/light surface (consent screens).
- Keep the two-color palette exact (`#38bdf8` / `#0f172a`).
- Preserve clear space and optical centering.
- Ship a simplified ring-only variant for ≤24px.

**Don't**
- Recolor outside the palette, or add a third color.
- Add gradients, shadows, glows, bevels, or 3D.
- Stretch, rotate, or skew the mark.
- Place the bare sky-blue mark on white (low contrast) — use a tile.
- Add a second accent element — the spark is the only one.

---

_Master assets live in `web/static/` once produced; this spec governs them._
