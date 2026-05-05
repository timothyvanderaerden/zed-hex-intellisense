use std::fs;
use zed_extension_api::{self as zed, Architecture, LanguageServerId, Os, Worktree};

/// GitHub repository that hosts both the WASM extension and the `hex-ls` binary releases.
const REPO: &str = "timothyvanderaerden/zed-hex-intellisense";

// ---------------------------------------------------------------------------
// Extension struct
// ---------------------------------------------------------------------------

/// The Hex.pm IntelliSense Zed extension.
///
/// On first use (or after a Zed restart), the extension downloads the correct
/// `hex-ls` prebuilt binary from the GitHub Releases page and caches it in
/// the extension's working directory.  Subsequent LS restarts within the same
/// Zed session reuse the in-memory `cached_binary_path`.
///
/// If `hex-ls` is already present on the user's `PATH` (e.g. via
/// `cargo install --path hex-ls`) that binary is used instead — it always
/// takes priority.
struct HexIntelliSenseExtension {
    /// Absolute path to the `hex-ls` binary downloaded during this session.
    cached_binary_path: Option<String>,
}

// ---------------------------------------------------------------------------
// GitHub Release asset metadata
// ---------------------------------------------------------------------------

/// Describes which GitHub Release asset to download and where it lands on disk
/// after Zed's `download_file` extracts it.
///
/// # Archive layout expected by Zed
///
/// | Platform | Archive      | Contents inside archive    | On-disk path after extraction           |
/// |----------|-------------|-----------------------------|-----------------------------------------|
/// | Unix     | `.tar.gz`    | `hex-ls-<target>/hex-ls`   | `hex-ls-<ver>/hex-ls-<target>/hex-ls`   |
/// | Windows  | `.zip`       | `hex-ls.exe`               | `hex-ls-<ver>/hex-ls.exe`               |
#[derive(Debug, PartialEq)]
struct ReleaseDetails {
    /// File name of the GitHub Release asset, e.g. `hex-ls-x86_64-unknown-linux-gnu.tar.gz`.
    asset_name: String,
    /// How Zed should interpret the downloaded file.
    file_type: zed::DownloadedFileType,
    /// Path of the directory Zed will extract the archive into, relative to the
    /// extension working directory.  Incorporates the release version so that
    /// old versions are automatically superseded.
    extract_dir: String,
    /// Final path to the `hex-ls` binary, relative to the extension working directory.
    binary_path: String,
}

impl ReleaseDetails {
    fn new(platform: Os, arch: Architecture, version: &str) -> Self {
        let arch_str = match arch {
            Architecture::Aarch64 => "aarch64",
            Architecture::X86 => "x86",
            Architecture::X8664 => "x86_64",
        };
        let os_str = match platform {
            Os::Mac => "apple-darwin",
            Os::Linux => "unknown-linux-gnu",
            Os::Windows => "pc-windows-msvc",
        };

        let asset_stem = format!("hex-ls-{arch_str}-{os_str}");

        let (asset_suffix, file_type) = match platform {
            Os::Mac | Os::Linux => ("tar.gz", zed::DownloadedFileType::GzipTar),
            Os::Windows => ("zip", zed::DownloadedFileType::Zip),
        };

        let asset_name = format!("{asset_stem}.{asset_suffix}");
        let extract_dir = format!("hex-ls-{version}");

        let binary_path = match platform {
            Os::Mac | Os::Linux => format!("{extract_dir}/{asset_stem}/hex-ls"),
            Os::Windows => format!("{extract_dir}/hex-ls.exe"),
        };

        Self {
            asset_name,
            file_type,
            extract_dir,
            binary_path,
        }
    }
}

// ---------------------------------------------------------------------------
// Extension implementation
// ---------------------------------------------------------------------------

impl HexIntelliSenseExtension {
    /// Resolves the path to the `hex-ls` binary, downloading it from GitHub
    /// Releases if it isn't already cached or available on `PATH`.
    fn language_server_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<String> {
        // 1. A user-installed binary on PATH always takes priority.
        if let Some(path) = worktree.which("hex-ls") {
            return Ok(path);
        }

        // 2. Reuse the path from a previous download within this Zed session.
        //    `fs::metadata` verifies the file still exists (e.g. not deleted
        //    by the user between LS restarts).
        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).map_or(false, |m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        // 3. Check GitHub for the latest release, download if needed.
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (platform, arch) = zed::current_platform();
        // Release tags are bare semver strings (e.g. "0.4.0"), not "v0.4.0".
        // Strip a leading 'v' defensively so the extension works with either
        // tag convention.
        let version = release
            .version
            .strip_prefix('v')
            .unwrap_or(&release.version)
            .to_string();

        let details = ReleaseDetails::new(platform, arch, &version);

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == details.asset_name)
            .ok_or_else(|| {
                format!(
                    "hex-ls release {version} has no asset named {:?}. \
                     Until prebuilt binaries are available you can build from source: \
                     cargo install --git https://github.com/{REPO} hex-ls",
                    details.asset_name,
                )
            })?;

        // Only download if the binary isn't already on disk from a previous
        // Zed session (the versioned directory acts as a cache key).
        if !fs::metadata(&details.binary_path).map_or(false, |m| m.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(&asset.download_url, &details.extract_dir, details.file_type)
                .map_err(|e| format!("Failed to download hex-ls: {e}"))?;

            // Unix archives don't preserve the execute bit through WASI —
            // mark the binary executable so the OS can run it.
            if matches!(platform, Os::Mac | Os::Linux) {
                zed::make_file_executable(&details.binary_path)
                    .map_err(|e| format!("Failed to make hex-ls executable: {e}"))?;
            }

            // Remove any directories left over from older versions so the
            // extension's working directory doesn't accumulate stale binaries.
            let entries =
                fs::read_dir(".").map_err(|e| format!("Failed to list working directory: {e}"))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("Failed to read directory entry: {e}"))?;
                if entry.file_name().to_str() != Some(&details.extract_dir) {
                    fs::remove_dir_all(entry.path()).ok();
                }
            }
        }

        self.cached_binary_path = Some(details.binary_path.clone());
        Ok(details.binary_path)
    }
}

impl zed::Extension for HexIntelliSenseExtension {
    fn new() -> Self {
        HexIntelliSenseExtension {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<zed::Command> {
        let command = self.language_server_binary_path(language_server_id, worktree)?;
        Ok(zed::Command {
            command,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(HexIntelliSenseExtension);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use zed_extension_api::{Architecture, DownloadedFileType, Os};

    use crate::ReleaseDetails;

    #[test]
    fn test_details_mac_aarch64() {
        let d = ReleaseDetails::new(Os::Mac, Architecture::Aarch64, "0.4.0");
        assert_eq!(d.asset_name, "hex-ls-aarch64-apple-darwin.tar.gz");
        assert_eq!(d.file_type, DownloadedFileType::GzipTar);
        assert_eq!(d.extract_dir, "hex-ls-0.4.0");
        assert_eq!(
            d.binary_path,
            "hex-ls-0.4.0/hex-ls-aarch64-apple-darwin/hex-ls"
        );
    }

    #[test]
    fn test_details_mac_x86_64() {
        let d = ReleaseDetails::new(Os::Mac, Architecture::X8664, "0.4.0");
        assert_eq!(d.asset_name, "hex-ls-x86_64-apple-darwin.tar.gz");
        assert_eq!(d.extract_dir, "hex-ls-0.4.0");
        assert_eq!(
            d.binary_path,
            "hex-ls-0.4.0/hex-ls-x86_64-apple-darwin/hex-ls"
        );
    }

    #[test]
    fn test_details_linux_x86_64() {
        let d = ReleaseDetails::new(Os::Linux, Architecture::X8664, "0.4.0");
        assert_eq!(d.asset_name, "hex-ls-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(d.file_type, DownloadedFileType::GzipTar);
        assert_eq!(d.extract_dir, "hex-ls-0.4.0");
        assert_eq!(
            d.binary_path,
            "hex-ls-0.4.0/hex-ls-x86_64-unknown-linux-gnu/hex-ls"
        );
    }

    #[test]
    fn test_details_linux_aarch64() {
        let d = ReleaseDetails::new(Os::Linux, Architecture::Aarch64, "1.0.0");
        assert_eq!(d.asset_name, "hex-ls-aarch64-unknown-linux-gnu.tar.gz");
        assert_eq!(d.extract_dir, "hex-ls-1.0.0");
        assert_eq!(
            d.binary_path,
            "hex-ls-1.0.0/hex-ls-aarch64-unknown-linux-gnu/hex-ls"
        );
    }

    #[test]
    fn test_details_windows_x86_64() {
        let d = ReleaseDetails::new(Os::Windows, Architecture::X8664, "0.4.0");
        assert_eq!(d.asset_name, "hex-ls-x86_64-pc-windows-msvc.zip");
        assert_eq!(d.file_type, DownloadedFileType::Zip);
        assert_eq!(d.extract_dir, "hex-ls-0.4.0");
        assert_eq!(d.binary_path, "hex-ls-0.4.0/hex-ls.exe");
    }

    /// Strip the leading 'v' from a tag so both "0.4.0" and "v0.4.0" work.
    #[test]
    fn test_v_prefix_strip() {
        let version_raw = "v0.4.0";
        let version = version_raw.strip_prefix('v').unwrap_or(version_raw);
        let d = ReleaseDetails::new(Os::Linux, Architecture::X8664, version);
        assert_eq!(d.extract_dir, "hex-ls-0.4.0");
    }
}
