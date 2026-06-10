# Vendored test fixture provenance

## komiku-the-calling.flac

- **Source:** "The calling" (track 1) from *The Adventure Goes On, Vol. 1* by
  **Komiku** (Loyalty Freak Music).
- **Origin:** <https://archive.org/details/komiku-the-adventure-goes-on-vol.-1>
- **License:** CC0 1.0 Universal (Public Domain Dedication) —
  <https://creativecommons.org/publicdomain/zero/1.0/>. No rights reserved; no
  attribution required (this note is courtesy, not obligation).
- **MusicBrainz:** artist `fca800c1-6fc3-4bfb-a5de-8c2398c27bc0`; release group
  `0520eabc-e117-48ce-b229-a6ea84c349aa`; track `7fe0e7ac-d373-4c34-9a47-f4582ed345ef`
  / recording — used so the Lidarr e2e's mock metadata matches a real artist.
- **Modifications:** trimmed to 2s, downmixed to mono, resampled to 8 kHz,
  cover art and tags stripped — purely to keep the fixture tiny (~29 KB). The
  audio content is irrelevant to the test (musefs splices metadata in front of
  positioned reads; the e2e asserts on tags and byte-invariance, not audio).

Used by `scripts/lidarr-e2e/` to drive a real Lidarr import → `OnReleaseImport`
→ the musefs custom script, against locally-mocked metadata/indexer/download
client.
