pub(crate) fn mock_context() -> tauri::Context<tauri::test::MockRuntime> {
    super::app_context()
}
