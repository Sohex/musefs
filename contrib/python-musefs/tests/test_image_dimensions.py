import struct

import pytest
from conftest import JPEG, PNG

from musefs_common import image_dimensions


def png(width, height, *, first_chunk=b"IHDR", length=13):
    """A PNG signature and IHDR chunk; the CRC is not checked, so it is zero."""
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", length)
        + first_chunk
        + struct.pack(">II", width, height)
        + bytes([8, 6, 0, 0, 0])
        + b"\x00" * 4
    )


def segment(marker, payload):
    return b"\xff" + bytes([marker]) + struct.pack(">H", len(payload) + 2) + payload


def sof(marker, width, height, *, components=b"\x01\x01\x11\x00"):
    """A start-of-frame segment: precision, height, width, then the component
    count and three bytes per component (one component by default)."""
    return segment(marker, bytes([8]) + struct.pack(">HH", height, width) + components)


APP0 = segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
SOS = segment(0xDA, b"\x01\x01\x00\x00\x3f\x00")


def jpeg(*segments):
    return b"\xff\xd8" + b"".join(segments) + SOS + b"\x00" * 8 + b"\xff\xd9"


def test_png_dimensions_come_from_ihdr():
    assert image_dimensions(png(640, 480)) == (640, 480)


def test_baseline_jpeg_after_an_app_segment():
    assert image_dimensions(jpeg(APP0, sof(0xC0, 1200, 800))) == (1200, 800)


def test_progressive_jpeg_after_a_fill_byte():
    # Any number of 0xFF fill bytes may precede a marker.
    assert image_dimensions(jpeg(APP0, b"\xff", sof(0xC2, 300, 200))) == (300, 200)


def test_a_standalone_marker_carries_no_length():
    assert image_dimensions(jpeg(b"\xff\xd0", sof(0xC1, 16, 9))) == (16, 9)


def test_a_non_frame_segment_in_the_sof_range_is_skipped():
    # DHT (C4) sits among the SOF markers and carries no dimensions.
    dht = segment(0xC4, b"\x00" * 17)
    assert image_dimensions(jpeg(dht, sof(0xC0, 64, 48))) == (64, 48)


@pytest.mark.parametrize(
    "data",
    [
        pytest.param(b"", id="empty"),
        pytest.param(b"not an image at all", id="garbage"),
        pytest.param(b"RIFF\x00\x00\x00\x00WEBPVP8 ", id="webp"),
        pytest.param(png(640, 480)[:20], id="png-truncated-in-ihdr"),
        pytest.param(png(640, 480)[:29], id="png-ihdr-without-its-crc"),
        pytest.param(png(640, 480, first_chunk=b"gAMA"), id="png-first-chunk-not-ihdr"),
        pytest.param(png(640, 480, length=12), id="png-ihdr-wrong-length"),
        pytest.param(png(0, 480), id="png-zero-width"),
        pytest.param(png(640, 0), id="png-zero-height"),
        pytest.param(png(2**31, 480), id="png-width-past-the-cap"),
        pytest.param(jpeg(APP0, sof(0xC0, 1200, 0)), id="jpeg-zero-height"),
        pytest.param(jpeg(APP0, sof(0xC0, 0, 800)), id="jpeg-zero-width"),
        pytest.param(jpeg(APP0), id="jpeg-image-data-before-any-frame"),
        pytest.param(b"\xff\xd8" + APP0 + sof(0xC0, 1200, 800)[:6], id="jpeg-sof-truncated"),
        pytest.param(
            jpeg(APP0, sof(0xC0, 1200, 800, components=b"\x00")), id="jpeg-sof-no-components"
        ),
        pytest.param(
            jpeg(APP0, sof(0xC0, 1200, 800, components=b"\x02\x01\x11\x00")),
            id="jpeg-sof-length-disagrees-with-component-count",
        ),
        pytest.param(
            jpeg(APP0, sof(0xC0, 1200, 800, components=b"")), id="jpeg-sof-without-component-count"
        ),
        pytest.param(b"\xff\xd8\xff\xe0\x7f\xff", id="jpeg-segment-past-the-end"),
        pytest.param(b"\xff\xd8\xff\xe0\x00\x01", id="jpeg-segment-length-below-two"),
        pytest.param(b"\xff\xd8\xff\xd9", id="jpeg-end-of-image-first"),
        pytest.param(b"\xff\xd8\xff\xff\xff", id="jpeg-only-fill-bytes"),
        pytest.param(b"\xff\xd8" + APP0 + b"\x00", id="jpeg-junk-where-a-marker-belongs"),
    ],
)
def test_unreadable_or_unstated_dimensions_are_none(data):
    assert image_dimensions(data) is None


def test_the_shared_fixture_headers_state_no_dimensions():
    # conftest's JPEG and PNG are magic bytes and padding. Every other suite
    # syncs them expecting the link geometry unset, so they must stay unreadable.
    assert image_dimensions(JPEG) is None
    assert image_dimensions(PNG) is None
