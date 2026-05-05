# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - Unreleased

## [0.4.0] - 2026-05-05

### Added
- **Auto-download of `hex-ls`**: the Zed extension now downloads the correct
  prebuilt `hex-ls` binary for the current platform directly from GitHub
  Releases.  No Rust toolchain is required on end-user machines.  A binary
  already on `PATH` is always preferred over the downloaded one.
- **GitHub Actions workflows**:
  - `ci.yml` — runs `cargo test -p hex-ls` and checks the WASM build on every
    push/PR to `main`.
  - `release.yml` — triggered by a `v`-prefixed semver tag push (e.g. `v0.5.0`); builds
    `hex-ls` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
    `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`;
    packages the results as `.tar.gz` / `.zip` archives; rebuilds
    `extension.wasm` and attaches it to the GitHub Release alongside the
    `hex-ls` archives.
- Asset-path construction tests in `src/lib.rs` covering all five target
  triples and the `v`-prefix stripping logic.

### Changed
- Simplified version completion filtering: removed the operator-detection
  (`~>` / `>=` / `digit`) and multi-regime `filter_text` logic in favour of a
  single `label.starts_with(partial)` server-side pre-filter.
- When a `text_edit` is present on a completion item, Zed uses the text from
  `text_edit.range.start` to the cursor as the filter input, which is always
  the full partial the user has typed (including any operator prefix such as
  `~> ` or `>= `).  Label-prefix matching is therefore sufficient and no
  `filter_text` overrides are needed.
- `preselect: true` is now consistently applied to the first matching item
  across all three item flavours (`~>`, `>=`, exact).
- Replaced the operator-detection unit tests with label-prefix-filter tests
  that directly document the new filtering contract.

### Fixed
- Transition bugs that occurred when deleting characters caused the cursor to
  move between operator and digit positions, sometimes showing the wrong set
  of items or a stale `filter_text`.

## [0.3.0] - 2025-05-01

### Added
- Version completions now return `CompletionList { is_incomplete: true, … }`
  instead of a plain array.  This tells Zed the list may change on every
  keystroke and causes it to re-request completions on deletions as well as
  insertions, greatly reducing the number of cases where the completion popup
  disappears during editing.
- Background pre-fetch of package metadata on `textDocument/didOpen` so that
  hover requests are served from cache without a network round-trip.

### Changed
- `text_edit` range now always spans from the opening `"` to the cursor
  (`ver_start … cursor`), overriding Zed's word-boundary guess and ensuring
  the selected item always replaces the entire partial version string.

## [0.2.0] - 2025-04-01

### Added
- Hover support: hovering over a package name atom shows the description,
  latest version, license, and links (HexDocs, Hex.pm, upstream repo).
- Hovering over a version string shows available versions grouped by minor
  series (`~> X.Y`).

### Changed
- HTTP responses are cached with a 5-minute TTL, making repeated completions
  and hovers instant.

## [0.1.0] - 2025-03-01

### Added
- Initial release.
- Package name completions after `{:` — queries the hex.pm search API and
  ranks results by download count.
- Version completions inside the quoted version string of a dep tuple —
  queries the hex.pm packages API and offers three formats per release:
  `~> X.Y` (semver-compatible), `>= X.Y.Z` (minimum), and `X.Y.Z` (exact pin).
- `~> X.Y` items are deduplicated to one per minor series (pointing at the
  latest patch release).
- Results limited to the 20 most recent releases to keep the completion list
  manageable.
- Completions are scoped exclusively to `mix.exs` files.
