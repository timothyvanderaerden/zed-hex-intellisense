use zed_extension_api::{self as zed, LanguageServerId, Worktree};

/// The Hex.pm IntelliSense extension.
///
/// This extension starts the `hex-ls` language server for Elixir files,
/// which provides completions for package names and version constraints
/// directly from the hex.pm package registry when editing `mix.exs`.
struct HexIntelliSenseExtension;

impl zed::Extension for HexIntelliSenseExtension {
    fn new() -> Self {
        HexIntelliSenseExtension
    }

    /// Returns the command to start the `hex-ls` language server.
    ///
    /// The `hex-ls` binary must be available in your `PATH`.  Install it by
    /// running the following from the root of this repository:
    ///
    /// ```sh
    /// cargo install --path hex-ls
    /// ```
    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<zed::Command> {
        // Resolve the full path to the `hex-ls` binary via PATH lookup.
        // Zed does not search PATH itself — it prepends the extension work
        // directory to a bare binary name, so we must return the absolute path.
        let command = worktree.which("hex-ls").ok_or_else(|| {
            "hex-ls not found in PATH. Install it with: cargo install --path hex-ls".to_string()
        })?;

        Ok(zed::Command {
            command,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(HexIntelliSenseExtension);
