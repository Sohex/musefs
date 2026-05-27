# Arbitrary Tag Support — Design

Date: 2026-05-27
Status: Approved, ready for planning

## Problem

A real-world FLAC in the library carries 45 tags. musefs must preserve *every*
tag on a backing file through scan → DB → synthesis, regardless of how obscure,
rather than only the handful of fields currently hard-coded per format.

## Current state

The data model is already generic and is **not** the bottleneck:

- The `tags` table is `(track_id, key, value, ordinal)` — arbitrary keys.
- `scan.rs::ingest` stores every probed tag unfiltered (lowercased key, ordinal
  per key).
- `mapping.rs::tags_to_inputs` / `tags_to_fields` and the resolution path pass
  *all* stored tags to synthesis. No changes needed here.

The loss happens entirely in the **format layer**, and only for some formats,
because each translates through a small fixed allowlist:

| Format            | Scan (read)                              | Synthesis (write)                          |
| ----------------- | ---------------------------------------- | ------------------------------------------ |
| FLAC, Ogg         | all Vorbis comments (generic) ✅          | all Vorbis comments (generic) ✅            |
| MP3, WAV (`id3 `) | **only ~9 known frames; rest dropped**   | known → text frame, unknown → `TXXX` ✅      |
| MP4 (m4a/m4b)     | **only ~7 atoms + trkn/disk; rest dropped** | **only ~7 atoms + trkn/disk; rest dropped** |

So MP3/WAV can *write* arbitrary tags but *forget* them on scan; MP4 drops them
on both sides. FLAC/Ogg already round-trip fully.

A consequence of musefs's architecture simplifies the goal: **synthesis always
emits the same format as the backing file** (a FLAC stays FLAC). "Lossless
round-trip" therefore always means *within a single format* — there is no
cross-format conversion to lose information.

## Goal

1. **Lossless round-trip, all formats** — every tag on the original reappears on
   the synthesized file.
2. **Canonical naming** — equivalent tags across formats normalize to a single
   canonical key so path templates work consistently regardless of source format.

### Scope of tag types

- **Text key=value:** full coverage — standard text frames, user-defined `TXXX`,
  MP4 `----` freeform, and all Vorbis comments.
- **Comments & lyrics:** preserved as text only. The ID3 `COMM`/`USLT` language
  code and short description are *not* preserved; written back with a default
  language and empty description (see Limitations).
- **Binary / extended frames** (e.g. `POPM` ratings) and non-standard custom
  4-char frames: out of scope, dropped on scan, documented as a limitation.

## Approach (chosen)

**Centralized canonical vocabulary + name-based passthrough.** Consolidate the
four scattered, hand-kept maps into one bidirectional vocabulary that is the
single source of truth; anything outside it round-trips through each format's
extension slot, keyed by its human-readable name.

Alternatives considered and rejected:

- *Minimal vocabulary + raw-native-key passthrough* — trivially lossless but
  leaks format-specific keys (frame ids, atom fourccs) into the DB and templates,
  defeating canonical naming.
- *Extend each format's maps in place* — smallest diff but perpetuates the
  existing read/write table duplication (the maps can drift) and provides no
  shared canonical space.

## Design

### 1. Canonical vocabulary module

New module `musefs-format/src/tagmap.rs` — the single source of truth. A static
table of canonical entries, each binding one canonical key to its native slot per
format:

```
canonical key   ID3 frame   MP4 atom   Vorbis field      kind
title           TIT2        ©nam       TITLE             text
albumartist     TPE2        aART       ALBUMARTIST       text
tracknumber     TRCK        trkn       TRACKNUMBER       number
date            TDRC        ©day       DATE              text
comment         COMM        ©cmt       COMMENT           text (special: COMM)
lyrics          USLT        ©lyr       LYRICS            text (special: USLT)
bpm             TBPM        tmpo       BPM               number
…               …           …          …                …
```

Coverage: all standard ID3v2.4 text frames, standard MP4 metadata atoms, plus
well-known `TXXX`/`----` conventions (MusicBrainz IDs, ReplayGain) so they
canonicalize to the same key across formats. **Canonical keys are the
Vorbis/beets field names (lowercased)**, so they double as the template field
names.

Public API (four bidirectional lookups the format modules call):

- `key_to_id3_frame(key) -> Option<&'static [u8; 4]>` / `id3_frame_to_key(id) -> Option<&'static str>`
- `key_to_mp4_atom(key) -> Option<&'static [u8; 4]>` / `mp4_atom_to_key(atom: &[u8; 4]) -> Option<&'static str>`

Vorbis needs only a tiny alias table (canonical ↔ field where they differ);
otherwise it is lower/upper-case identity. These pairs **replace** today's
`key_to_frame`/`frame_to_key` (`mp3.rs`) and `meta_key`/`atom_to_key` (`mp4.rs`),
eliminating the read/write duplication.

### 2. Passthrough rule

Any tag whose key is not in the vocabulary is user-defined and round-trips
through the format's extension slot, keyed by its human name:

- ID3 → `TXXX`, description = key
- MP4 → `----`, mean = `com.apple.iTunes`, name = key
- Vorbis → field = key (uppercased)

The DB therefore only ever stores readable keys (canonical or the user's own
name), never raw fourcc codes.

### 3. Read (scan) changes per format

`tracks`, `mapping.rs`, `tags_to_inputs`/`tags_to_fields` are unchanged.

- **FLAC / Ogg (Vorbis):** already generic. Route field names through the Vorbis
  alias table so `comment`/`lyrics`/etc. land on canonical keys consistently.
- **MP3 / WAV (`mp3::read_tags`, shared by WAV's `id3 ` chunk):** for each frame —
  text frame → `id3_frame_to_key` → canonical (NUL-split multivalue, as today);
  `TXXX` → key = description; `COMM` → `comment` (text only); `USLT` → `lyrics`
  (text only). Non-standard custom frames and binary frames (e.g. `POPM`) are
  skipped (documented limitation).
- **MP4 (`mp4::read_tags`):** known atom → canonical; `©cmt`/`©lyr` →
  `comment`/`lyrics` (now in vocab); `trkn`/`disk` → numbers (as today); `----`
  freeform → key = its `name`, value = data (new). Other/binary atoms skipped
  (documented).

### 4. Write (synthesis) changes per format

- **FLAC / Ogg (`vorbiscomment::build`):** already writes all; route canonical
  keys → Vorbis field via the alias table (uppercased).
- **MP3 (`build_id3v2_segments`):** key in vocab → text/number frame (as today);
  `comment` → `COMM` (default language `XXX`, empty description); `lyrics` →
  `USLT` (same); everything else → `TXXX` (as today). WAV shares this via its
  `id3 ` chunk. New: COMM/USLT emission.
- **MP4 (`build_udta`):** key in vocab → native text/number atom;
  `comment`/`lyrics` → `©cmt`/`©lyr`; everything else → `----` freeform (mean
  `com.apple.iTunes`, name = key). New: today's silent drop of unknown keys is
  replaced by `----` emission. A larger `udta` is already accommodated by the
  existing `stco`/`co64` offset-patching, so no new plumbing is required.

### 5. Multi-value

Unchanged ordinal model. `TXXX` / `----` emit one frame/atom per value; reads
accumulate repeats into ordinals — consistent with how Vorbis and ID3 text
frames already behave.

## Limitations (to document in README)

A "Tag handling" subsection in the README states what round-trips losslessly
(all text tags via the vocabulary; user-defined tags via `TXXX`/`----`/Vorbis
fields; comments & lyrics text) and the explicit lossy edges:

- ID3 `COMM`/`USLT` language code and description are not preserved; written back
  as language `XXX`, empty description. Multiple comments/lyrics distinguished
  only by those collapse to one.
- MP4 `----` `mean` is normalized to `com.apple.iTunes` on write.
- Binary / extended frames (e.g. `POPM` ratings; `APIC` beyond the existing
  dedicated art path) and non-standard custom 4-char frames are not round-tripped
  — they are dropped on scan.

## Testing

Following the existing format-test patterns and TDD discipline:

- **Vocabulary unit tests:** every table entry round-trips both directions
  (`key → native → key`); assert no duplicate canonical keys or native codes.
- **Per-format round-trip tests:** build a tag set mixing canonical fields, a
  user-defined tag, a multi-value tag, and a comment; synthesize → re-parse →
  assert equality. New cases specifically for ID3 `TXXX`/`COMM`/`USLT` reads and
  MP4 `----` read+write (the previously-dropped paths).
- **Scan integration:** ingest fixtures with many tags (the 45-tag FLAC scenario,
  an MP3 with `TXXX`, an M4A with `----`); assert all survive into the DB.
- **Regression:** existing format and FUSE e2e tests stay green; synthesized
  files remain structurally valid (byte-length/structure assertions already
  present per module).

## Out of scope

- Cross-format conversion (musefs never converts formats).
- Lossless preservation of `COMM`/`USLT` qualifiers, MP4 `----` non-iTunes
  `mean`, binary/extended frames, and non-standard custom frames (see
  Limitations).
- Schema changes — the `tags` table already supports arbitrary keys.
