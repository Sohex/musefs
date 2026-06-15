# Introduction

A read-only FUSE filesystem that presents a re-tagged, reorganized view of
your music library — without modifying or duplicating a single byte of the
original audio. Fix tags, art, and folder structure in a SQLite store; the
mount shows a clean library while your files stay exactly as they are.

## What it's for

- **A clean view of a messy library.** Your files keep their on-disk chaos;
  the mount presents one consistent, template-driven tree for players and
  media managers.
- **Tag editing without touching files.** Edit the SQLite store (directly,
  or via the [beets plugin](https://sohex.github.io/musefs/integrations/beets.html),
  [Picard plugin](https://sohex.github.io/musefs/integrations/picard.html), or
  [Lidarr integration](https://sohex.github.io/musefs/integrations/lidarr.html)) and the mounted view
  updates live — no remount, no rewrite, no re-rip anxiety.
- **Lossless-by-construction experimentation.** Change your tags, try a different
  organization scheme, new cover art — the originals are physically
  read-only to the mount. Backing up a current library is as easy as copying the db file.
- **Hash-stable by construction.** The mount never rewrites a byte, so each
  backing file's checksum is exactly what it was the day it arrived — anything
  verified by hash keeps verifying, and anything you're seeding keeps seeding,
  however aggressively you retag and reorganize the view on top.

> **Note:** This project was built with AI. The general workflow was to use the [superpowers](https://github.com/obra/superpowers) skills to provide a framework. Claude Opus was used to write plans and specs which were then implemented by another model, primarily MiMo v2.5.
>
> One of my goals in building this project was to "vibe code" something that was decisively not slop. I believe I've realized that objective and I hope that you take the project on its merits.
>
> If you disagree, please let me know! I'd love to know where I came up short so I can improve things.

## Status

All five formats ship with embedded cover art and binary-tag preservation.
The serve path has been through a performance/concurrency hardening pass for
real-world player and media-manager access against large libraries on
HDD/SSD/NFS, and the parsers are continuously fuzzed. beets, Picard, and
Lidarr plugins ship in [`contrib/`](https://sohex.github.io/musefs/integrations/overview.html). See the
[CHANGELOG](https://sohex.github.io/musefs/changelog.html) for history.

Deeper reading: [ARCHITECTURE.md](https://sohex.github.io/musefs/architecture/overview.html) for how it works,
[CONTRIBUTING.md](https://sohex.github.io/musefs/contributing/setup.html) for the development workflow.
