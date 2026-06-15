# Supported formats

musefs synthesizes fresh metadata for each supported container while serving
the original audio bytes verbatim. Each format has its own page for the exact
synthesis behavior and lossy edges.

| Format | Extensions | What is synthesized |
| ------ | ---------- | ------------------- |
| [FLAC](flac.md) | `.flac` | Regenerates the metadata blocks; preserves `STREAMINFO`/`SEEKTABLE` bit-exact |
| [MP3](mp3.md) | `.mp3` | Regenerates the ID3v2.4 tag; audio frames (incl. Xing/LAME) untouched |
| [M4A](m4a.md) | `.m4a`, `.m4b` | Rebuilds the `moov` atom, patching chunk offsets; `mdat` served verbatim |
| [Ogg](ogg.md) | `.ogg`, `.oga`, `.opus` | Regenerates header pages; audio pages verbatim, only page seq/CRC patched in place |
| [WAV](wav.md) | `.wav` | Regenerates the RIFF front (`LIST`/`INFO` + embedded ID3v2); `data` payload verbatim |
