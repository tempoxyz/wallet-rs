//! Common test utilities for tempo-cards CLI tests.
//!
//! Keep this module minimal so all test targets can include it without lint allowances.

/// Create a test command for tempo-cards with proper environment variables set.
pub(crate) fn test_command(temp_dir: &tempfile::TempDir) -> std::process::Command {
    tempo_test::make_test_command(
        assert_cmd::cargo::cargo_bin!("tempo-cards").to_path_buf(),
        temp_dir,
    )
}
