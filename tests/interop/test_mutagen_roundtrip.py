import json
import os

import mutagen
import mutagen.flac
import mutagen.id3
import mutagen.mp4
import soundfile

# Mirrored byte-for-byte from musefs-core/tests/interop_emit.rs (COVR_JPEG/COVR_PNG).
COVR_JPEG = b"\xff\xd8\xff\xe0interop-jpeg-cover"
COVR_PNG = b"\x89PNG\r\n\x1a\ninterop-png-cover"

# Mirrored from interop_emit.rs's PICTURES, in ordinal order: (data, mime,
# picture_type, description, width, height, depth, colors).
PICTURES = [
    (COVR_PNG, "image/png", 4, "Back Cover", 300, 200, 8, 256),
    (COVR_JPEG, "image/jpeg", 6, "Disc", 640, 480, 24, 0),
]

# Every fixture emit_interop_fixtures writes a manifest row for.
MANIFEST_FILES = {"out.flac", "out.mp3", "out.m4a", "out.ogg", "out.wav", "out_rifx.wav"}

# The samples interop_emit.rs's RIFX fixture holds, big-endian on disk.
RIFX_SAMPLES = [0x0102, -2, 300, -32768, 32767, 5, 6, 7]


def _is_rifx(path):
    with open(path, "rb") as fh:
        return fh.read(4) == b"RIFX"


def _read_tag(path, key):
    """Read a single tag value from an audio file using mutagen."""
    # RIFX (big-endian WAVE): mutagen's RIFF reader refuses it outright, so read
    # the LIST/INFO chunk through libsndfile, which reads RIFX natively.
    if _is_rifx(path):
        with soundfile.SoundFile(path) as f:
            return getattr(f, key) or None

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
    # Sorted lists, not sets: a duplicated manifest row must fail too.
    assert sorted(row["file"] for row in manifest) == sorted(MANIFEST_FILES)
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
    # Sorted lists, not sets: a duplicated manifest row must fail too.
    assert sorted(row["file"] for row in manifest) == sorted(MANIFEST_FILES)
    for row in manifest:
        synth_length = row["synth_audio_length"]
        # Every fixture has audio, so a zero-length payload is a regression, not
        # a case to skip.
        assert synth_length > 0, f"{row['file']}: synthesized audio payload is empty"

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


def test_rifx_is_served_as_rifx_and_decodes_like_its_source():
    """A big-endian RIFX source is served as RIFX (#770), and libsndfile decodes
    the served file to exactly the source's samples. Decoding them as the
    fixture's values also proves the preserved `fmt ` is read big-endian."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    src = os.path.join(base, "src_rifx.wav")
    out = os.path.join(base, "out_rifx.wav")
    assert _is_rifx(src) and _is_rifx(out)
    src_samples, src_rate = soundfile.read(src, dtype="int16")
    out_samples, out_rate = soundfile.read(out, dtype="int16")
    assert src_samples.tolist() == RIFX_SAMPLES
    assert (out_samples.tolist(), out_rate) == (RIFX_SAMPLES, src_rate)


def test_binary_frames_survive():
    """Binary tag frames written to the DB survive the mount and are readable
    by mutagen: POPM/UFID as semantic fields, PRIV/GEOB and MP4 ---- byte-for-byte."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "binary_manifest.json")) as fh:
        bm = json.load(fh)

    # ── MP3 (ID3) ──
    mp3 = bm["mp3"]
    id3 = mutagen.id3.ID3(os.path.join(base, mp3["file"]))

    # Presence before the value check, for the clearer failure message.
    priv = [f for f in id3.getall("PRIV") if f.owner == mp3["priv_owner"]]
    assert priv, "PRIV frame missing"
    assert priv[0].data == mp3["priv_data"].encode("ascii"), "PRIV data changed"

    geob = id3.getall("GEOB")
    assert len(geob) == 1, f"{len(geob)} GEOB frames, one was written"
    assert (geob[0].mime, geob[0].filename, geob[0].desc, geob[0].data) == (
        mp3["geob_mime"],
        mp3["geob_filename"],
        mp3["geob_desc"],
        mp3["geob_data"].encode("ascii"),
    ), "GEOB frame changed"

    popm = id3.getall("POPM")
    assert len(popm) == 1, f"{len(popm)} POPM frames, one was rebuilt"
    # Rebuilt from the rating/playcount text tags, which carry no owner.
    assert popm[0].email == "", f"POPM owner {popm[0].email!r}"
    assert int(popm[0].rating) == int(mp3["rating"]), f"rating {popm[0].rating} != {mp3['rating']}"
    assert int(popm[0].count) == int(mp3["playcount"]), (
        f"playcount {popm[0].count} != {mp3['playcount']}"
    )

    ufid = [f for f in id3.getall("UFID") if f.owner == "http://musicbrainz.org"]
    assert ufid, "MusicBrainz UFID missing"
    assert ufid[0].data == mp3["mb_trackid"].encode("ascii"), "musicbrainz_trackid changed"

    # ── MP4 (----) ──
    mp4 = bm["mp4"]
    f = mutagen.mp4.MP4(os.path.join(base, mp4["file"]))
    vals = f.tags.get(mp4["freeform_key"]) if f.tags else None
    assert vals is not None and len(vals) == 1, f"freeform atom {mp4['freeform_key']}: {vals!r}"
    assert bytes(vals[0]) == mp4["freeform_data"].encode("ascii"), "---- payload changed"


def test_m4a_multi_cover_art():
    """Every track_art row is served as a covr `data` atom (iTunes convention);
    mutagen reads them back in ordinal order, bytes intact."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    rows = [r for r in manifest if r.get("covr_count")]
    assert rows, "no manifest row declares cover art"
    for row in rows:
        f = mutagen.mp4.MP4(os.path.join(base, row["file"]))
        covr = f.tags.get("covr") if f.tags else None
        assert covr is not None, f"{row['file']}: no covr tag"
        assert len(covr) == row["covr_count"]
        assert bytes(covr[0]) == COVR_JPEG
        # Presence-only as a check on the stored mime: `covr` can only say PNG
        # or not-PNG, and synthesis serves every non-PNG mime (an empty one
        # included) as JPEG. The PNG line below is the one that tells a right
        # mime from a wrong one; the full mime string is asserted on FLAC and
        # ID3 in test_flac_and_id3_pictures_carry_every_link_field.
        assert covr[0].imageformat == mutagen.mp4.MP4Cover.FORMAT_JPEG
        assert bytes(covr[1]) == COVR_PNG
        assert covr[1].imageformat == mutagen.mp4.MP4Cover.FORMAT_PNG


def test_flac_and_id3_pictures_carry_every_link_field():
    """Every `track_art` field a picture block can carry comes back as the link
    declared it, in ordinal order. FLAC's PICTURE block carries all of them;
    ID3 `APIC` has no geometry fields, so the MP3 side checks the rest."""
    base = os.environ["MUSEFS_INTEROP_DIR"]

    flac = mutagen.flac.FLAC(os.path.join(base, "out.flac"))
    served = [
        (p.data, p.mime, p.type, p.desc, p.width, p.height, p.depth, p.colors)
        for p in flac.pictures
    ]
    assert served == PICTURES

    apics = mutagen.id3.ID3(os.path.join(base, "out.mp3")).getall("APIC")
    served = [(a.data, a.mime, a.type, a.desc) for a in apics]
    assert served == [picture[:4] for picture in PICTURES]
