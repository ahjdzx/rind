# Rind user manual

English | [中文](../USAGE.md)

For users: how to install it, how to use it, what every command does, where things are kept,
and what to do when something goes wrong.
To understand why it is designed this way, see [DESIGN.md](DESIGN.md); to know what it
guarantees and what it does not, see [SAFETY.md](SAFETY.md).

## 1. Requirements

- macOS. Development and verification happen on macOS 26 (Tahoe) / Apple Silicon; older systems
  and Intel machines are untested.
- Running it does not require a full Xcode (the interface uses Metal shaders compiled at runtime).
- Building from source requires Rust **1.89+** (verified locally with stable 1.99).

## 2. Install

**Download the DMG (recommended)**: grab the latest `Rind-v*.dmg` from
[Releases](https://github.com/ahjdzx/rind/releases) and drag Rind into Applications.
The DMG is built by GitHub Actions on Apple Silicon (arm64) and is **not signed or notarized
with a Developer ID** — the first launch needs right-click → Open, after that it behaves
normally. Requires macOS 14+.

**Build from source**:

```sh
git clone git@github.com:ahjdzx/rind.git
cd rind

cargo build --release -p rind-cli      # the CLI; output at target/release/rind
cargo build --release -p rind-app      # the desktop app; output at target/release/rind-app
```

To build a DMG yourself: `scripts/package-dmg.sh`, output in `target/dmg/Rind-v<version>.dmg`.
Signing, notarization and a Homebrew cask are the remaining work, tracked in
[ROADMAP](../ROADMAP.md) (Chinese).

## 3. First launch

The desktop app opens on the **Overview** page:

1. The top strip is disk capacity; below it is the **Smart scan** button — it scans for orphaned
   files and cleanable items in one pass and estimates how much could be freed.
2. The sidebar on the left holds every page: **Overview / Apps / Orphans / Cleanup / Space /
   Launch items**, with **History / Settings** at the bottom.
3. To first see how it judges things, open demo mode — its data is fake and nothing on your
   system is touched:

   ```sh
   RIND_DEMO=1 cargo run -p rind-app
   ```

On the command line, start with the read-only commands:

```sh
rind status            # disk capacity + number of uninstallable apps (add --scan to also sum up orphans and cleanable space)
rind apps              # list installed apps (--all also lists the ones that come with macOS)
rind leftovers WeChat  # what WeChat left behind, and why each file belongs to it (read-only)
rind protected         # the places Rind will never touch
```

## 4. The desktop app

| Page | What it does | Notes |
| --- | --- | --- |
| **Overview** | Disk capacity, smart scan, entry points to every page | A scan can be cancelled halfway; results are read-only and never check anything for you |
| **Apps** | Uninstall an app and its leftovers | Every file gets a reason and a confidence; items in protected areas show no checkbox |
| **Orphans** | Files left behind by deleted apps | Only the "the owner is definitely gone" part is pre-checked; when Spotlight is unavailable the whole page degrades to "list only, nothing pre-checked" |
| **Cleanup** | Caches, logs, package-manager caches, project build artifacts | Grouped by category; expensive-to-rebuild items are listed but not checked; build artifacts are only suggested once a project has been untouched long enough |
| **Space** | Which folders take up the most room, where the big files are | Read-only; folders containing media libraries / wallets cannot be selected |
| **Launch items** | Login items and background jobs | Only items belonging to the current user can be disabled / enabled / removed |
| **History** | What Rind has moved | One batch at a time; put back a whole batch or single items |
| **Settings** | Appearance, language, cleanup preferences, protected list | Appearance can follow the system / force light / force dark, and the language can be Chinese / English; both choices are remembered (`<data-dir>/appearance` and `<data-dir>/language`) and reused on the next launch; forcing an appearance switches the native title bar along with it. The protected list is read-only — no setting can override it |

Before anything moves, Rind first lists "what will be moved, how much that frees, and what
stays"; only after you confirm does it act. When it is done it tells you where things went
(the Trash) and prints a hint like `rind restore <id>`.

## 5. The command line

Every command accepts two global flags:

```
--json          JSON output, for scripts and AI agents
--lang zh|en    language (defaults to RIND_LANG, then LANG)
```

### 5.1 Reading

| Command | Description |
| --- | --- |
| `rind apps [--all]` | List installed apps |
| `rind leftovers <APP> [--no-size]` | What one app left behind, with a reason and a confidence per file (read-only) |
| `rind protected` | The places it will never touch |
| `rind history` | What Rind has moved |
| `rind status [--scan]` | Overview; `--scan` also scans orphans and cleanable items (slower) |
| `rind analyze [PATH] [--depth N] [--top N] [--large-min SIZE]` | Folder sizes and the list of large files (read-only) |
| `rind launch list` | Every launch item |

`<APP>` can be a name, a bundle id or a path, is case-insensitive, and a partial value is fine.
With someone watching (interactive mode), a fuzzy name lists every match and asks you to pick
one; with nobody watching (`--yes` / `--json`) only a full match is accepted.

### 5.2 Uninstall

```sh
rind uninstall WeChat                 # list the plan first, then ask once
rind uninstall WeChat --dry-run       # only say what it would do; nothing is touched
rind uninstall WeChat --yes           # do not ask
rind uninstall WeChat --keep-app      # move leftovers only, keep the app itself
rind uninstall WeChat --quit          # if the app is running, ask it to quit first (wait 5 s)
rind uninstall WeChat --force-quit    # force-quit it if it refuses to leave
rind uninstall WeChat --all           # include what Rind did not pick for you (protected areas still stay put)
```

### 5.3 Cleanup

```sh
rind orphans                       # orphaned leftovers (read-only)
rind orphans --trash --dry-run     # rehearse: what would move, what would be refused, how much would be freed
rind orphans --trash --yes         # really clean (only what Rind suggests, by default)
rind orphans --trash --path ~/Library/Caches/com.foo --yes   # clean one specific path

rind clean                         # caches / logs / build artifacts (read-only)
rind clean --category dev_artifacts --min-age 60
rind clean --trash --dry-run
rind clean --trash --yes
```

`--category` accepts: `app_caches`, `logs`, `browser_caches`, `xcode`, `package_caches`,
`dev_artifacts`, `needs_review`.
`--project-root` sets where to look for projects (defaults include `~/work`, `~/Projects`,
`~/Developer`), and `--min-age` is "only suggest cleaning after this many days untouched".

### 5.4 Move arbitrary files

```sh
rind trash ./build --dry-run       # rehearse
rind trash ./build ~/old-notes --yes
```

Paths in protected areas, and photo / music libraries and browser profiles with wallets inside
the folder, are refused (the rehearsal and the real run agree).
Note: when you explicitly name a folder, Rind only applies its safety net for media libraries
and wallets — items inside it that are "in use / need admin" will not stop the move. What moves
is that folder, and it can still be put back as a whole.

### 5.5 Put things back

```sh
rind history                       # find the id of that move (the first few characters are enough)
rind restore 3f9a                  # put it back where it came from
```

Putting back works batch by batch: if one item is no longer in the Trash, or something else now
sits at its original path, that item is skipped with a stated reason and the rest go back as
usual.

### 5.6 Launch items

```sh
rind launch list
rind launch disable com.example.agent        # disable (reversible, moves no files)
rind launch enable  com.example.agent        # enable again
rind launch remove  com.example.agent --dry-run
rind launch remove  com.example.agent --yes  # bootout first, then move the plist to the Trash
```

The label must be complete. Items under `/Library` and `com.apple.*` items are listed but
cannot be acted on.

## 6. For scripts and AI agents

- **Rehearse first**: every command that moves things supports `--dry-run`; in `--json` output
  `moved` is what would be moved, `refused` what was refused, `freed` how many bytes that frees.
  The rehearsal and the real run give the same judgement.
- **Use full names when nobody is watching**: under `--yes` / `--json`, a fuzzy match will not
  guess for you.
- **`rind status --scan --json`** is a good entry point: one call gives disk, app count, orphan
  and cleanable-space totals.
- **Do not use `--all` instead of judgement**: `--all` also includes "possible / shared /
  unreadable", which is a safety net for humans, not a default.
- Exit codes: `0` means success; `1` means a runtime error or some items were refused / failed;
  `2` means an argument error; `3` means there was nothing to move or the user cancelled.
  A rehearsal with any refusal is also `1`. With `--json`, errors are on stdout as
  `{"error":...}`; otherwise they are on stderr.

## 7. Where files live

| Location | What it is |
| --- | --- |
| `~/Library/Application Support/Rind/history.jsonl` | History: the manifest of every move (one record per line) |
| `~/Library/Application Support/Rind/` | App icon cache and similar |
| The Trash (`~/.Trash`; on external volumes `<volume>/.Trashes/<uid>`) | Where moved things actually sit |
| `rules/*.toml` (in the source tree, baked in at build time) | Per-app cleanup rules |

Rind has no account, no telemetry and no network requests; its only network activity is you
running `git pull`.

## 8. Environment variables

| Variable | Effect |
| --- | --- |
| `RIND_LANG` | Language of the UI and command output (`zh` / `en`); in the desktop app it beats the choice remembered in Settings, and demo mode honours it too; on the command line it beats `LANG` |
| `RIND_DEMO=1` | Start the desktop app with sample data; nothing on your system is touched |
| `RIND_PAGE=space` | Development aid: open one page directly at launch |
| `RIND_AUTOSCAN=1` | Development aid: run a smart scan on launch |
| `RIND_APPEARANCE` | Startup appearance of the desktop app (`system` / `light` / `dark`), beating the choice remembered in Settings; demo mode honours it too |
| `RIND_HOME` + `RIND_ROOT` | Point the home directory and the system root somewhere else (used by integration tests); both must be set for this to take effect |

## 9. FAQ

**Why are some items not pre-checked?**
Only items that are "certain / likely to belong to this app", that no other app shares, and
whose size is known get pre-checked. Anything uncertain is listed so you can judge it yourself;
items needing admin rights are listed but never moved.

**Why does something show as "can't be read"?**
Usually macOS privacy protection (TCC) is blocking a location like a sandbox container. Grant
Full Disk Access to your terminal or to Rind and scan again. Rind never counts an unreadable
folder as 0 bytes.

**Why can I not uninstall while the app is running?**
Use `--quit` (waits 5 s) or `--force-quit`, or quit it in the UI first. Uninstalling a running
app may let it write a pile of files right back.

**Is "12 GB can be freed" real?**
That is computed from actual usage (after de-duplicating APFS clones and hard links), but the
space only comes back **once you empty the Trash**.

**How do I confirm what it actually did?**
`rind history --json` has the full manifest of every move (including the real path inside the
Trash). If you want to look before anything happens, always add `--dry-run`.

**Will it delete my chat history / photos?**
Nothing in the protected areas can be overridden by any selection; deep-buried things like
photo libraries, music libraries and wallet profiles are also stopped (6 levels down).
But remember: Rind only moves them to the Trash, and emptying the Trash is what really deletes
them — after that nobody can bring them back.

**How do I remove Rind itself cleanly?**
Do not reach for `rm -rf` here; use Rind itself:

```sh
rind uninstall Rind --yes       # if it lives in /Applications
rind trash ~/Library/Application\ Support/Rind --yes   # History and caches
```

## 10. When something goes wrong

- Start with `rind history`: it records what Rind has done, and putting things back also starts
  there.
- Security problems (a way around the protected areas, something moved that should not have
  been) should be reported privately per [SECURITY.md](../../SECURITY.md) — please do not open
  a public issue.
- Anything else: when opening an issue, include your macOS version, chip, `rind --version`,
  reproduction steps, and (redacted) `rind status --json`.
