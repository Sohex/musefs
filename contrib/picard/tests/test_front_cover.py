from musefs._core import front_cover


def test_no_images_returns_none(fake_metadata):
    assert front_cover(fake_metadata()) is None


def test_returns_first_front_image_data_and_mime(fake_metadata, fake_image):
    img = fake_image(b"JPEGBYTES", "image/jpeg", front=True)
    data, mime = front_cover(fake_metadata(images=[img]))
    assert data == b"JPEGBYTES"
    assert mime == "image/jpeg"


def test_skips_non_front_images(fake_metadata, fake_image):
    back = fake_image(b"BACK", "image/png", front=False)
    front = fake_image(b"FRONT", "image/jpeg", front=True)
    data, mime = front_cover(fake_metadata(images=[back, front]))
    assert data == b"FRONT"


def test_all_non_front_returns_none(fake_metadata, fake_image):
    back = fake_image(b"BACK", "image/png", front=False)
    assert front_cover(fake_metadata(images=[back])) is None
