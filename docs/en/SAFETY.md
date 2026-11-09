# Rind's safety model

English | [中文](../SAFETY.md)

This is a tool that moves your files, so this is where it states why it dares to. To report a
vulnerability, see [SECURITY.md](../../SECURITY.md).

## 1. The promise

**Rind never deletes anything outright.** Every move goes through `NSFileManager.trashItem`
into the Trash; every move is recorded in History with the spot it actually landed in, and can
be put back item by item. The only truly irreversible thing is one action of yours: emptying
the Trash — you do that in the Finder, Rind does not do it for you. It also only acts when you
press the button (no automatic cleaning, no background process).

## 2. The single exit

Uninstall, orphans, cleanup, space, launch items, `rind trash` — every feature that moves
things funnels into one function, `rind_core::remove::trash_items`. It does six things in a
fixed order:

1. **Resolve the real path**: turn the selection into its actual location on disk (resolving
   symlinks in the ancestors, restoring the real casing).
2. **Check protected areas**: the path itself, its ancestors, and media libraries / wallet
   profiles up to 6 levels down inside directories.
3. **Refuse outer folders**: if an outer folder contains anything that must not move
   (protected / macOS's own / needs admin / in use), the whole outer folder is refused.
4. **Record intent first**: write "I am about to move this" into History (`history.jsonl`); if
   that write fails, not a single file is moved.
5. **Move**: one `trashItem` per file, checking that what moved is the object that was
   inspected (device number + inode).
6. **Write back**: record where it actually landed in the Trash; if that fails, stop, move
   nothing further, and report exactly the part that already happened.

Any new feature that moves files has to go through here — this is the one rule code review is
most reluctant to grant an exception to.

## 3. Protected areas

Things in protected areas **cannot be overridden by any option, any flag, any `--all`**:

iCloud Drive / CloudStorage, Keychains, `~/.ssh`, GPG keys, Mail, Messages, Safari and its
Cookies, Contacts, photo and music libraries (the `*.photoslibrary` / `*.musiclibrary`
extensions, wherever they live), iPhone / iPad backups, the Desktop / Documents / Downloads
folders themselves, system paths (`/System`, `/usr`, `/bin`, …), and browser profiles with
crypto wallet extensions (MetaMask and the like).

The check is by **real on-disk identity**, not string comparison: case variants (APFS is
case-insensitive by default), symlinks in the middle, interleaved `..` are all resolved to the
real path before deciding. `rind protected` lists every rule.

## 4. History and putting things back

- Records are written to `~/Library/Application Support/Rind/history.jsonl`, one per line; one
  batch of moves shares a single record.
- Putting back checks every item: it is really in the Trash, the original path is a clean
  absolute path, it still resolves to the recorded location, and it is not in a protected
  area. If any check fails, that item is skipped with a stated reason and the rest go back as
  usual.
- Putting back never overwrites existing files (`RENAME_EXCL`): if something already sits at
  the original path, it is not touched.
- Cross-volume put-backs (an external drive, say) copy first, verify file counts and byte
  counts, and only then remove the copy in the Trash.

## 5. Why launch items are harder

Launch items involve `launchctl`, which reads the plist from disk — there is a time gap
between "checking" and "acting". Rind handles it like this:

- Symlinks are neither followed nor accepted: if `~/Library/LaunchAgents` itself, or any level
  between it and your home directory, is a symlink, the whole user domain is not listed.
- Before acting, the plist is reopened with `O_NOFOLLOW` and re-checked: an ordinary file,
  owned by the current user, not writable by group or others, at most 1 MB, not a deeply
  nested bomb (the same tame check shared with Info.plist), with the on-disk `Label`
  byte-identical to the scan and the program unchanged.
- The file's identity is recorded (device + inode + size + mtime / ctime + content hash) and
  compared once before `bootout`, once after `bootout`, and once more right before the actual
  `rename`; any mismatch means no move, and anything already unloaded is `bootstrap`ped back.
- Labels may only contain `[A-Za-z0-9._-]`; items under `/Library` and `com.apple.*` items are
  never acted on.

## 6. Known limits (the trade-offs, stated up front)

These are not "later"; they are the explicit boundaries of this version:

| Limit | Explanation |
| --- | --- |
| Depth of directory inspection | Before moving a directory, Rind looks 6 levels down, up to 200,000 entries; a media library / wallet buried deeper will not be caught. This is the compromise between "catch the vast majority" and "cleanup still works". |
| Bare symlinks | Moving a **link** that points into a protected area is allowed — the link is not the target, moving it does not take away what it points at, and History can put it back. |
| The `launchctl` moment | Between Rind's last check and `launchctl` reading the file itself there is still a very small window; that is a property of its interface. |
| Needing admin rights | Items under `/Library`, system-domain launch items and the like are listed but never moved; Rind does not ask for elevation. |
| Full Disk Access | Without it, locations like sandbox containers cannot be read and are reported as "can't be read" — never shown as 0 bytes. |
| `--all` | It also carries "possible / shared / unreadable" items, but it still cannot cross a protected area. |
| Emptying the Trash | Space only comes back after the Trash is emptied; until then everything can go back. |

## 7. How to verify it yourself

If you would rather not read the code, look at it like this:

```sh
rind protected                    # what it promises never to touch
rind leftovers WeChat             # why it believes these files belong to WeChat
rind uninstall WeChat --dry-run   # rehearse: what moves, what is refused, what is freed
rind orphans --trash --dry-run    # same, for orphaned leftovers
rind clean --trash --dry-run
rind history --json               # what it has actually done
```

The rehearsal and the real run give the same judgement. If you want to go deeper: every safety
check has a corresponding test (`cargo test -p rind-core`, 213 of them), including deliberately
constructed attack cases — symlinks swapped mid-flight, a same-named look-alike directory
swapped in, a file rewritten in place with its timestamps set back, a deeply nested plist bomb.
The tests never call the real `launchctl` and never touch your real files: they run in their
own temporary environment.
