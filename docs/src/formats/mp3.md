# MP3

How musefs scans and synthesizes MP3 files (`.mp3`) and their ID3v2 metadata.
For the segment model these layouts plug into, see
[the segment model](../architecture/serving.md#the-segment-model). The ID3v2 builder
described here is shared with WAV's embedded `id3 ` chunk — see
[WAV](wav.md).

## What round-trips

- **Canonical text tags** (`title`, `artist`, `albumartist`, `date`,
  `tracknumber`, …) map to their standard ID3v2 text frames (`TIT2`, `TPE1`,
  `TPE2`, `TDRC`, `TRCK`, …) via the shared vocabulary
  (`musefs-format/src/tagmap.rs`). NUL-separated multi-value frames yield one
  tag row per value and are re-emitted NUL-separated in a single frame.
- **Vocabulary `TXXX` keys** (ReplayGain fields, MusicBrainz album/artist
  ids) round-trip through `TXXX` frames with their fixed, exact-case
  descriptions (e.g. `MusicBrainz Album Id`).
- **Unmapped standard text frames** round-trip keyed by their own frame id: a
  `TSSE` (or a legacy v2.3 `TYER`) comes back as the same frame inside the
  synthesized tag.
- **Other user-defined keys** round-trip as `TXXX` frames keyed by their own
  description, original casing preserved.
- **Comments and lyrics** (`COMM`/`USLT`): one tag row per frame. A frame with a
  placeholder language (`XXX`/`und`/empty) and no descriptor folds to the shared
  `comment`/`lyrics` key; one carrying a real language or descriptor is keyed
  `id3:COMM:<lang>:<desc>` / `id3:USLT:<lang>:<desc>` so per-language or
  description-keyed frames stay distinct, and both fields are restored on
  synthesis.
- **Ratings and play counts**: a `POPM` frame is promoted at scan time to
  `rating` (the raw 0–255 byte) and `playcount` (omitted when 0) text tags,
  and rebuilt as a `POPM` frame on synthesis.
- **MusicBrainz track id**: a `UFID` frame with the `http://musicbrainz.org`
  owner is promoted to `musicbrainz_trackid` and rebuilt with the same owner.
- **Opaque binary frames, byte-exact**: `PRIV`, `GEOB`, `SYLT`, `MCDI`,
  URL (`W***`) frames, non-MusicBrainz `UFID`s, and unknown frames are
  captured verbatim (frame id + raw body) and re-emitted streamed from the DB
  (`BinaryTag` segments) — never held in memory.
- **Embedded pictures** (`APIC`): MIME type, picture type, and description
  round-trip; image bytes are stored content-addressed and streamed.

## Where the tags are

ID3v2.4 lets a tag be prepended to the audio, appended after it, or both
([ID3v2.4.0 structure §5](https://id3.org/id3v2.4.0-structure)). musefs finds
the tags at both ends of the file, and none of their bytes is served as audio
([#767](https://github.com/Sohex/musefs/issues/767),
[#768](https://github.com/Sohex/musefs/issues/768)):

- **Prepended tags**: the whole run of consecutive ID3v2 tags at the start of
  the file. Each is stepped over by its declared size, plus the footer when a
  v2.4 header declares one, whatever its version: the header has the same
  shape in every version, and the spec's rule for a version you do not
  understand is to ignore the tag. A run of more than 64 tags is refused as
  malformed.
- **Appended tags**: an appended tag is required to end in a 10-byte `3DI`
  footer (§3.4), so the locator looks for one at the end of the file and walks
  backwards from it. A footer counts only if every field agrees: the `3DI`
  magic, version `$04`, a revision other than `$FF`, the footer flag set and no
  undefined flag bits (`%abcd0000`), a synchsafe size, and an `ID3` header at
  the offset that size gives, whose version, flags and size bytes match the
  footer's. Consecutive appended tags are walked the same way, up to the same
  64-tag ceiling.
- **An ID3v1 trailer**: 128 bytes beginning `TAG`. It may follow the appended
  tags, which is where §5 puts "tags from other tagging systems", or precede
  them, as a writer that appends at end of file produces. At most one is
  recognised.

An appended tag has to begin after the audio's frame sync. A footer whose tag
would start inside the prepended tags, or on the sync itself, describes bytes
that are not after the audio, so it is not taken as an appended tag. The
MPEG frame-sync requirement at the start of the audio is unchanged.

The scan reads the file's last 138 bytes, enough for an ID3v1 trailer and the
footer in front of it, and when a footer declares a tag it reads exactly the
extent the footer declares, rather than the MPEG payload. That read is held to
the same 64 MiB probe ceiling as the front of the file: a file whose appended
tags reach further back than that fails the scan as unparseable.

Two things at the end of a file are not recognised:

- **The `SEEK` frame** (frames §4.29) is not followed. It points at a further
  tag within the stream, and musefs looks for tags only at the two ends of the
  file. The layout the frame exists for, a prepended tag plus an appended one,
  is found by the footer search regardless.
- **APEv2 tags.** An APEv2 tag at the end of an MP3 stays inside the audio
  region, and so does an appended ID3v2 tag in front of it, because the footer
  search starts at the end of the file and stops at the APE footer. An ID3v1
  trailer after an APEv2 tag is still excluded.

## Which tag wins

When a file carries more than one ID3v2 tag, their contents are merged in file
order: the prepended run first, then the appended tags. The rule is §5's:

> For every new tag that is found, the old tag should be discarded unless the
> update flag in the extended header (section 3.2) is set.

- **A later tag without the update flag replaces everything before it.** This
  also fits §5's own prepend-and-append layout, in which the prepended tag
  holds "all vital information" for streaming and the appended tag is the one
  a reader that reaches the end is meant to keep.
- **A later tag with the update flag overrides only what it carries.** §3.2
  defines the flag as "the present tag is an update of a tag found earlier in
  the present file or stream. If frames defined as unique are found in the
  present tag, they are to override any corresponding ones found in the earlier
  tag." Uniqueness comes from the
  [ID3v2.4.0 frames](https://id3.org/id3v2.4.0-frames) document, applied at the
  grain the store keeps:
  - text, `TXXX`, `COMM` and `USLT` frames override by store key, compared
    case-insensitively as the store compares keys. A `TXXX` is therefore
    unique by its description, and a `COMM`/`USLT` by its language and
    descriptor;
  - a `POPM` overrides `rating` and `playcount` together, since both come from
    one frame;
  - an `APIC` overrides the pictures with the same description;
  - `UFID`, `AENC`, `RVA2` and `EQU2` override by their owner or identification
    string;
  - the frames allowed once per tag (`MCDI`, `ETCO`, `MLLT`, `SYTC`, `RVRB`,
    `PCNT`, `RBUF`, `POSS`, `OWNE`, `SEEK`, `ASPI`, and every URL frame but
    `WXXX`, `WCOM` and `WOAR`) override by frame id;
  - any other binary frame is kept from both tags, with byte-identical copies
    collapsed. That includes the frames unique by a descriptor musefs does not
    decode (`GEOB`, `WXXX`, `SYLT`, `USER`, `ENCR`, `GRID`), which can
    consequently appear twice.
- **Position decides which tag is later.** §5 finds a prepended tag first and
  appended tags by scanning backwards, but §3.2 defines an update against a tag
  "found earlier in the present file or stream", and in a stream, the case the
  flag was designed for, tags arrive in file order. So several appended tags are
  merged front to back, the same as a prepended run.
- **A prepended run follows the same rule.** ID3v2.3 and earlier have no update
  flag, so in a run of older tags the last tag wins outright; a v2.4 tag in the
  run can still mark itself an update. A player that reads only the first tag
  will show the first tag's metadata for such a file.
- **A tag musefs cannot read discards nothing.** A tag the allocation guard
  refuses (see below) contributes no tags, and it does not wipe out the tags
  before it either: replacing readable metadata with none would lose
  information the file does carry.

## Lossy edges

- The synthesized tag is always **ID3v2.4**, regardless of the source tag's
  version (v2.2/v2.3 tags are parsed but never re-emitted as such). It is a
  single prepended tag: the backing file's own tags, at either end, are never
  carried through.
- A `COMM`/`USLT` frame folded to the shared `comment`/`lyrics` key (placeholder
  language, no descriptor) is re-emitted with language `XXX` and an empty
  descriptor, so a source `und` placeholder comes back as `XXX`. Frames carrying
  a real language or descriptor are preserved (see above).
- `POPM`: the owner ("email to user") field is dropped by design. Multiple
  `POPM` frames collapse to one (first rating wins, last parseable play
  count wins); counters above `u32::MAX` clamp to 4 bytes.
- **ID3v1 is not read.** A file whose only tag is ID3v1 scans with no tags
  (populate the DB via beets/Picard instead). Its trailer is excluded from the
  audio region all the same, so the synthesized file does not carry it.
- The audio locator refuses only a prepended header that fails the spec's
  detection pattern (a `$FF` version byte, or a synchsafe size byte with the
  high bit set), with a controlled `Malformed` error rather than mask-decoding
  an invalid offset. Tags using unsynchronisation or an extended header still
  scan, since their declared size already covers the audio boundary.
- Scan-time tag extraction is skipped for a tag, by a deliberate
  denial-of-service guard (see below), when it has a major version other than
  2–4, unsynchronisation, an ID3v2.3 extended header, a malformed ID3v2.4
  extended header, non-zero frame flags (compression/encryption), malformed
  synchsafe size fields, or `CHAP`/`CTOC` chapter frames. A well-formed v2.4
  extended header is read, for its update flag. Such files still mount and
  serve; the skipped tag just contributes no scanned tags.
- ID3v2.2 binary frames are not extracted (3-char ids; text and art still
  parse). `APIC` width/height are not recorded at scan time.
- An `APIC` picture type outside the standard `0`–`20` range (the `id3`
  crate's `Undefined(u8)` variant can exceed 20) is clamped to `0` (`Other`)
  at scan time, matching the store's `track_art.picture_type` `CHECK`.

## How synthesis works

`mp3::synthesize_layout` (`musefs-format/src/mp3.rs`) emits a fresh ID3v2.4
tag followed by the untouched audio:

```text
 offset 0
 ┌──────────────────────────────────────────────┐ ┐
 │ █ ID3v2.4 header (10 bytes)          (Inline) │ │
 │ █ text / TXXX / COMM / USLT frames   (Inline) │ │ generated
 │ █ rebuilt POPM / UFID frames         (Inline) │ │ ID3v2.4
 │ █ frame header + ▒ opaque body    (BinaryTag) │ │ tag
 │ █ APIC framing + ▒ image bytes     (ArtImage) │ │
 ├──────────────────────────────────────────────┤ ┘
 │ ░ MPEG audio incl. Xing/LAME,  (BackingAudio) │
 │ ░ verbatim                                    │
 └──────────────────────────────────────────────┘
 EOF     █ inline-generated   ▒ DB-streamed   ░ untouched backing
```

1. `Inline` — the 10-byte tag header, all text/`TXXX`/`COMM`/`USLT` frames,
   and the rebuilt `POPM`/`UFID` frames. Frame sizes are synchsafe-bounded;
   oversized frames fail synthesis rather than emit a corrupt tag.
2. Per opaque binary frame: an inline frame header + a `BinaryTag` segment
   streaming the body from the DB (empty payloads are skipped — they would
   fail layout validation).
3. Per picture: inline `APIC` framing + an `ArtImage` segment streaming the
   image bytes.
4. `BackingAudio` — the audio region located at scan time: everything after
   the run of prepended ID3v2 tags and before the first trailing tag, whether
   that is an appended ID3v2 tag or an ID3v1 trailer (see
   [Where the tags are](#where-the-tags-are)), anchored by an MPEG frame-sync
   check. The Xing/LAME info frame is an MPEG frame, so it travels with the
   audio untouched.

## Quirks & invariants

- **The OOM guard** (`id3v2_alloc_safe`): the `id3` parser crate eagerly
  allocates a frame's declared size (v2.3 sizes are plain 32-bit — up to
  4 GiB), so musefs validates every frame bound itself before handing a
  buffer to the crate, and refuses tags it cannot validate. Each tag is handed
  over sliced to its own extent, so nothing past its end is in reach. Found and
  locked in by the `mp3` fuzz target; the conservative skips listed under
  "Lossy edges" are this guard.
- Byte-identical audio and tag round-trip stability are asserted by
  `musefs-format/tests/proptest_mp3.rs` and the mutagen interop suite
  (`musefs-core/tests/interop_emit.rs`), which includes a backing file with
  tags at both ends.
