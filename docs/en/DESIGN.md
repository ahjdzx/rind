# Rind design document

English | [中文](../DESIGN.md)

For people who want to change it, learn from it, or know "why is it like this". The technical
layers and data flow are in [ARCHITECTURE.md](../ARCHITECTURE.md) (Chinese); this document covers the
product and interaction trade-offs.

## 1. What it is, and what it is not

Rind is a macOS uninstaller and cleaner: when you uninstall an app, it finds the leftovers the
app scattered around `~/Library`, **explains why each file belongs to it and how sure it is**,
and then moves them all to the Trash.

**Goals**

- Make "why is this file going away" visible before anything happens.
- Make every move undoable (History puts it back in one click).
- When unsure, do not act — and say exactly where the uncertainty is.

**Non-goals**

- Not antivirus / anti-malware, no real-time protection.
- No resident background monitoring (no daemon watching your files). Rind works only when you
  press the button.
- No features that need the network: no account, no telemetry, no cloud rules.
- Does not touch system-level installs (macOS updates, SIP, `/System`).
- Does not "one-click optimize" for you: the only thing it does automatically is judge; before
  acting it always shows you the plan.

## 2. The four design principles

1. **Explainable.** Every candidate carries `Evidence` (bundle id, embedded components, App
   Groups, signing Team, name, rules, pkg receipts, orphan signals …) and `Confidence` (certain
   / likely / possible). In the UI that is the "why it counts" line.
2. **Undoable.** Every deletion stops at the Trash, and History records where it actually
   landed. The only irreversible operation is "empty the Trash", which the user does in the
   Finder; Rind does not do it for them.
3. **Conservative defaults.** Pre-checked means certain/likely + no other app shares it + size
   known + not protected + not running. Everything else is listed but not checked.
   Expensive-to-rebuild cleanup items (IDE indexes, browser-engine downloads, package-manager
   stores) are also listed but not checked.
4. **The rehearsal is the real run.** `--dry-run` runs the same checks: what moves, what is
   refused, how much is freed, item for item, on both sides. The confirmation dialog in the UI
   shows exactly this plan.

## 3. Information architecture

Sidebar fixed at 228px, in three groups:

```
Overview                   ← the single entry page: disk, smart scan, links to every page

Uninstall & clean
  Apps      uninstall an app and its leftovers
  Orphans   files deleted apps left behind
  Cleanup   caches / logs / package managers / build artifacts
  Space     who takes up the most room + large files
  Launch    LaunchAgents / LaunchDaemons

Bottom
  History   what was moved, and putting it back
  Settings  appearance, language, cleanup preferences, protected list
```

Why "one overview + five action pages" instead of splitting by object (apps / files /
services): when users sit down, what they have in mind is "free up some space", not "I want to
deal with orphaned leftovers". The Overview answers "how much can be freed right now"; the
specific pages answer "free it from where".

## 4. Page responsibilities and key interactions

### Overview

- Top: disk capacity bar (`disk::usage`, total / used / available).
- Middle: **Smart scan** — runs the orphan and cleanup scans in one go and sums up "how much
  can be freed". It can be cancelled halfway; whatever was already found is still shown.
- Bottom: entry-point cards for every page. Scan results are read-only; they never flip your
  checkboxes as a side effect.

### Apps

- The list is sorted by name; each row is icon + name + version + size + source (App Store /
  Homebrew / unknown).
- Opening one shows "what this app left behind": each item has a path, a size, a reason and a
  confidence.
- Items in protected areas show no checkbox, only the reason; items needing admin rights are
  equally uncheckable.
- The bottom toolbar always shows "N selected · X MB total" and the primary button; before
  acting, a confirmation dialog lists what moves, what is refused, and what is freed.

### Orphans

- Grouped by "the app it came from"; group headers state who the owner was and why it is
  considered gone (Spotlight cannot find it, it is not on disk, the cask is uninstalled …).
- An app just dragged to the Trash does not count as "installed" — that is the most common
  scenario.
- When Spotlight is unavailable the whole result degrades: everything is capped at "possible",
  nothing is pre-checked, and a notice at the top explains why.

### Cleanup

- Grouped by category (app caches / logs / browser caches / Xcode / package managers / build
  artifacts / needs review).
- "Needs review" is never suggested; build artifacts need corroborating files (a `Cargo.toml`
  next to `target/` …), and are only suggested once the project has been untouched for more
  than `--min-age` days.
- Items whose owner is running are marked "in use" and not checked.
- Project roots can be added and removed in Settings (defaults include `~/work`, `~/Projects`,
  `~/Developer`).

### Space

- The tree shows `--depth` levels, top N items per level; the rest collapses into "(other)";
  to look deeper, analyze a subdirectory again.
- Large files are listed to the right/below.
- A folder containing a photo / music library or a wallet profile is **entirely unselectable**,
  with "contains something that must not move: <path>" spelled out — without that, moving the
  outer folder would take them along.
- Nodes that have not yet been verified count as "unselectable" (better to over-block).

### Launch items

- One per row: label, program, whether it is an orphan, run state (from `/bin/launchctl`),
  whether RunAtLoad / KeepAlive.
- Only the current user domain can be acted on; disable / enable is reversible (`launchctl
  disable|enable`); remove is bootout + the plist into the Trash.

### History

- One move is one row (uninstall / orphans / cleanup / files / launch items); open it to see
  the manifest.
- "Put back" is a whole-batch operation, reported item by item: skipped items state the reason
  (no longer in the Trash, something already at the original path, the location cannot be put
  back from History).

### Settings

- Language (Chinese / English, remembered in `<data-dir>/language`), cleanup preferences
  (project roots etc.).
- The **protected list** is read-only: nothing listed there can be overridden by any setting.
  It lives on this page to answer "what do you promise not to touch".

## 5. Visuals and copy

- **Color is semantic only**: `Primary` (emphasis / primary button), `Info`, `Success`,
  `Warning` (in use, uncertain), `Danger` (refusals, dangerous actions), `Neutral`
  (explanatory). The meanings are fixed; nothing is repurposed to look nicer.
- **Light and dark are first-class**: every color goes through theme tokens (gpui-component's
  `Theme`); only decorative fixed values like the white text on a letter-avatar background are
  hand-picked. It supports "follow system / light / dark", and forcing one uses
  `App::set_window_appearance` so the native window frame and title bar switch along and no
  corner is left with the wrong color. The Settings choice is persisted
  (`<data-dir>/appearance`).
- **One component set**: page headers, cards, icon tiles, progress bars, confidence labels,
  notices, empty states, stat blocks and the bottom toolbar all come from `ui.rs`; pages
  compose them, they do not invent their own styles.
- **Empty states always say what to do next**: not "no data" but actionable lines like "no
  orphaned leftovers found — deleted apps usually leave the most".
- **Progress is visible and cancellable**: a scan shows the current phase and how many items it
  has looked at, and long tasks carry a cancel button next to them.
- **Copy style**: Chinese first, plain-English-style short sentences, no jargon. State the
  consequences ("it goes to the Trash; space is only really freed once it is emptied"), no
  exclamation marks, never scare and never sweet-talk.
- **Bilingual coverage**: every enum, button and error goes through a `rind-i18n` key, and
  tests guarantee both the Chinese and English tables are filled.

## 6. Data flow (the UI side)

```
GPUI UI ──(background_executor)──▶ rind-core (synchronous API + rayon)
     ▲                                  │
     └──── ScanProgress callbacks (progress / current phase) ────┘
```

- Core types are all `Send + Sync + Clone + Serialize`: UI, CLI and `--json` share the same
  ones.
- Scanning runs on the background executor; the UI only receives progress callbacks;
  cancellation goes through a shared `cancel` flag.
- Between UI and core sits a `Services` layer: real mode calls core, `RIND_DEMO=1` swaps in an
  in-memory sample backend, so the whole interaction can be walked through without touching
  the system.

## 7. Extension points

| What you want to add | Where to start |
| --- | --- |
| Cleanup paths for an app | `rules/<bundle-id>.toml` (can only add evidence, can never bypass a protected area) |
| A new cleanup category | `CleanupCategory` in `cleanup.rs` + the category copy in i18n + the Settings explanation |
| New uninstall evidence | `EvidenceKind` in `matcher.rs` + the scoring rules + i18n |
| A new page | `Page` in `model.rs` + `*_page.rs` + the sidebar; data goes through `Services` |

Any new feature that moves things has to go through the single exit `remove::trash_items` (see
[SAFETY.md](SAFETY.md), section 2).
