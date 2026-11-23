# 027 — Free space on every machine, and cleaning what rebuilds itself

> Written for: whoever adds a place the cleanup may delete from, and whoever
> reads a machine's card in the panel.

The panel shows every machine's RAM and CPU. This spec adds the number that
actually stopped work on the reference deployment: free space on each
machine's system drive. It also adds a way to get space back that deletes
only what rebuilds itself, and proves the gain by measuring the drive.

## What happened (2026-11-18)

The Windows machine's system drive, 112 GB, reached **5 MB free**. Nothing
said so. What the person saw was failures elsewhere, each with its own
message:

- a deploy stopped with "not enough space on the disk", before it changed
  anything;
- two package installs stalled for 25 minutes;
- an agent's own temporary writes failed.

The panel was open on that machine the whole time, showing its RAM and CPU
every few seconds. It was the fourth time in two months on that machine
(also 2026-09-29, 2026-10-24 and 2026-10-26), each time found the same way:
something broke.

Getting the space back was a lesson too:

- **What rebuilds itself was most of it.** 8.5 GB came back from caches
  alone: Go's module cache, the GPU shader cache, and old copies of build and
  package caches whose tools had already moved to another drive.
- **The biggest single folder was data.** 4.5 GB of temporary files from
  another program held three database dumps. "Temp" is not a promise that a
  file is disposable.
- **A folder can stop being where it looks.** The same evening, another
  cleanup moved two cache folders to the data drive and left a junction at
  the old path. A cleanup that followed the junction would have deleted on
  the other drive, where nothing said "cache".
- **Counting what you delete is not measuring what you freed.** The cleanup
  counted 8.06 GB and the drive gained 8.47 GB. They agreed because both
  were measured; a script that only adds up deletions cannot tell a file it
  deleted from a file the system kept open.

## What it does

1. **Shows free space** on the system drive of every machine, on its card,
   with a level: ok, low or critical. The system drive is `C:` (the
   machine's own `%SystemDrive%`) on Windows and `/` on Linux and macOS.
2. **Says so without anyone opening a card.** The summary line at the top of
   the panel counts the machines that are low, and the daemon writes a log
   line whenever a machine's level changes, and at startup if the drive is
   already low.
3. **Lists what can be reclaimed** on a machine, read only: each place on a
   fixed list, with its size and when it was last written. It also names the
   biggest folders it will not touch, so the person knows where the rest of
   the space went.
4. **Cleans what the person ticks**, from that list only. The result shows
   two numbers: what it counted and what the drive gained. When they
   disagree by much, it says so.

### The levels

Decided in one place (`oxdesk/src/disk.rs`), so the card, the summary and the
log agree:

| Level | Below | On the 112 GB drive |
|---|---|---|
| critical | 3% of the drive, at most 2 GB | 2 GB |
| low | 10% of the drive, at most 10 GB | 10 GB |
| ok | -- | -- |

The caps keep a large drive from being called low with hundreds of GB free.
The drive of the incident was critical at 5 MB and still low at 9.4 GB after
the first cleanup. That was right: a single install had filled it a week
earlier.

## The list of what may be deleted

The cleanup deletes from fixed paths only, never from a search. A path is on
the list only if what is in it comes back by itself.

| Group | What | Ticked by default |
|---|---|---|
| Rebuilds at no cost | temporary files older than 7 days; GPU shader caches; OxDesk's own leftovers (old binaries an update kept, rotated logs) | yes |
| Rebuilds with a download | developer caches (Go modules and builds, npm, pnpm, pip, browser builds for test tools); the recycle bin | no |
| Never | Downloads, Documents, Desktop; synced folders; installed applications; another program's data, even under Temp; restore points and `Windows.old`; anything that needs administrator rights | -- |

Where a tool chooses its own cache location, the tool is asked (`go env
GOMODCACHE`, `npm config get cache`), not the default path. On the machine
of the incident, the npm and bun caches had already moved to the data drive,
and the default paths held only old copies.

What needs administrator rights (the Windows component store, the Windows
Update download cache) is not run. The listing prints the command, for a
person to run in an elevated terminal.

## The rules the cleanup follows

- **Fixed paths only.** A new place goes on the list in a change of its own,
  with the reason it rebuilds itself.
- **Never through a junction or a symbolic link.** A listed path that is, or
  passes through, a link is reported as "moved elsewhere, not touched".
- **A file in use stays.** It is counted as "left in use", not as a failure,
  and the listing says how much that was.
- **The gain is measured on the drive**: free space before, free space
  after.
- **Listing never deletes, and cleaning needs a click** on a list the person
  has seen. Through the local API it is accepted only from this machine, like
  every other command (`guard.rs`).

## Reaching another machine

The numbers come with the rest of a machine's detail: one command per
system, run locally or over SSH, whose output gained two fields at the end
(`probe::detail_cmd`). A machine on an older build sends the shorter line,
and its card shows a dash for Disk.

Listing and cleaning run on the machine that owns the drive, as `oxdesk
disk-list` and `oxdesk disk-clean`, printing JSON, reached the same way the
clipboard is today. When SSH stops being the only way between machines,
these verbs move with the others.

## Order of work

1. Free space on the card, the level, the summary line and the log line:
   **done** (2026-11-18). Measured on the reference deployment, each against
   an independent reading: Windows `C:` 20.96 of 111.3 GB (`Get-PSDrive`
   says 20.96), Ubuntu `/` 29.60 of 218.0 GB (`df -h` says 30G of 218G), the
   Mac 16.84 of 228.3 GB.
2. Listing and cleaning on Windows, where it happened: **done**
   (2026-11-19), in `oxdesk/src/disk_clean.rs` and the panel's "Free up
   space" dialog. Seen on the reference Windows machine:
   - the junctions another cleanup had left under Playwright's folder were
     skipped, and only the one real folder (437 MB) was offered; the npm
     cache, which lives on the data drive, read "moved elsewhere";
   - the first cleanup counted 47.40 MB, the daemon measured 47.43 MB
     gained, and a reading taken outside it 47.49 MB; 22.9 MB were held
     open (shader caches, a temp file) and stayed, said as such;
   - a second run found only those held files and said "nothing was
     deleted", not success;
   - the "elsewhere" walk first ran out of time on two folders of small
     files and never reached AppData, where the big folders were; it now
     walks six folders at a time and marks a size it could not finish as
     "at least";
   - a machine on an older build answers with an error object, and the
     dialog says that machine needs updating first.
3. The Linux and macOS lists.
4. A desktop notification when a machine turns critical, for the machine
   nobody has the panel open on.

## Where the idea came from

The cleanup's model comes from an open-source skill for agents,
[`MickeyWzt/c-drive-cleaner-skill`](https://github.com/MickeyWzt/c-drive-cleaner-skill)
(MIT): audit first, a fixed list, an age threshold, deeper groups only when
asked, and comparing what was counted with what the drive gained. The design
and the code here are ours.

## See also

- [`017`](017-product.md) — the product this belongs to
- [`../.claude/rules/verify-what-arrived.md`](../.claude/rules/verify-what-arrived.md)
  — why the gain is measured and not added up
