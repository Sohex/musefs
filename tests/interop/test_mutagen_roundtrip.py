import json
import os

import mutagen
import mutagen.id3
import mutagen.mp4


def _read_tag(path, key):
    """Read a single tag value from an audio file using mutagen."""
    # M4A: read via real mutagen.mp4.MP4 (the interop fixture includes mdhd +
    # stsd so mutagen's stream-info parser can open the file).
    if path.endswith(".m4a"):
        atom = {
            "\xa9nam": "\xa9nam",
            "\xa9ART": "\xa9ART",
            "title": "\xa9nam",
            "artist": "\xa9ART",
        }.get(key)
        if atom is None:
            return None
        f = mutagen.mp4.MP4(path)
        v = f.tags.get(atom) if f.tags else None
        return str(v[0]) if v else None

    # MP3: mutagen.File requires valid MPEG frames; fall back to ID3 directly.
    if path.endswith(".mp3"):
        id3_key = {"title": "TIT2", "artist": "TPE1"}.get(key)
        if id3_key is None:
            return None
        try:
            f = mutagen.id3.ID3(path)
            tag = f.get(id3_key)
            if tag is not None:
                return str(tag[0])
        except Exception:
            pass
        return None

    f = mutagen.File(path, easy=True)
    assert f is not None, f"mutagen could not open {path}"
    vals = f.get(key)
    if vals:
        return vals[0]
    # Fallback: some containers (e.g. WAV/ID3) may not expose the easy key.
    f2 = mutagen.File(path)
    if f2 is not None and f2.tags is not None:
        for tag_key in (key, key.upper(), {"title": "TIT2", "artist": "TPE1"}.get(key)):
            if tag_key and tag_key in f2.tags:
                v = f2.tags[tag_key]
                return str(v[0]) if isinstance(v, list) else str(v)
    return None


def test_ecosystem_reads_synthesized_tags():
    """Verify mutagen reads synthesized tags from fixtures."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    assert manifest, "empty manifest"
    for row in manifest:
        path = os.path.join(base, row["file"])
        title = _read_tag(path, "title")
        artist = _read_tag(path, "artist")
        assert title == row["title"], f"{row['file']}: title {title!r} != {row['title']!r}"
        assert artist == row["artist"], f"{row['file']}: artist {artist!r} != {row['artist']!r}"


def test_synthesized_preserves_source_audio_payload():
    """Verify synthesized outputs contain the original audio payload bytes.

    For non-Ogg formats the audio region is served verbatim from the backing
    file via BackingAudio segments, so the bytes must be identical. Ogg audio
    pages are renumbered and CRC patched; this Python interop test verifies
    the emitted byte count for Ogg while Rust tests cover Ogg payload serving.
    """
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    assert manifest, "manifest.json is empty — emit_interop_fixtures may have failed"
    for row in manifest:
        synth_length = row["synth_audio_length"]
        if synth_length == 0:
            continue

        synth_path = os.path.join(base, row["file"])
        with open(synth_path, "rb") as f:
            f.seek(row["synth_audio_offset"])
            synth_payload = f.read(synth_length)
        assert len(synth_payload) == synth_length, (
            f"{row['file']}: expected {synth_length} audio bytes, got {len(synth_payload)}"
        )
        assert synth_length == row["source_audio_length"], (
            f"{row['file']}: synthesized audio length differs from source"
        )
        if row["ogg_payload_only"]:
            continue

        src_path = os.path.join(base, row["source_file"])
        with open(src_path, "rb") as f:
            f.seek(row["source_audio_offset"])
            src_payload = f.read(row["source_audio_length"])
        assert synth_payload == src_payload, f"{row['file']}: synthesized audio differs from source"
