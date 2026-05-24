#[test]
fn core_error_is_constructible_from_db_error() {
    fn assert_send<T: Send>() {}
    assert_send::<musefs_core::CoreError>();
}
