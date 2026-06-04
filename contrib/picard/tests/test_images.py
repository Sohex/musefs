from musefs._common.sync import ArtImage
from musefs._core import images


def test_no_images_returns_empty_list(fake_metadata):
    assert images(fake_metadata()) == []


def test_all_images_returned_in_order_with_types(fake_metadata, fake_image):
    front = fake_image(b"FRONT", "image/jpeg", front=True, maintype="front")
    back = fake_image(b"BACK", "image/png", front=False, maintype="back")
    assert images(fake_metadata(images=[front, back])) == [
        ArtImage(b"FRONT", "image/jpeg", 3, ""),
        ArtImage(b"BACK", "image/png", 4, ""),
    ]


def test_unknown_maintype_front_image_maps_to_3(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=True, maintype="obi")
    assert images(fake_metadata(images=[img]))[0].picture_type == 3


def test_unknown_maintype_non_front_maps_to_0(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=False, maintype="obi")
    assert images(fake_metadata(images=[img]))[0].picture_type == 0


def test_missing_maintype_falls_back_to_front_detection(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=True)  # no maintype attribute
    assert images(fake_metadata(images=[img]))[0].picture_type == 3


def test_unsavable_image_skipped(fake_metadata, fake_image):
    hidden = fake_image(b"X", "image/jpeg", can_be_saved_to_tags=False)
    keep = fake_image(b"Y", "image/png", maintype="front")
    out = images(fake_metadata(images=[hidden, keep]))
    assert [i.data for i in out] == [b"Y"]


def test_comment_becomes_description(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", maintype="booklet", comment="page 1")
    a = images(fake_metadata(images=[img]))[0]
    assert a.picture_type == 5
    assert a.description == "page 1"
