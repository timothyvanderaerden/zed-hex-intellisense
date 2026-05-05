# Hex.pm IntelliSense — Zed Extension

A [Zed](https://zed.dev) extension that provides IntelliSense for [hex.pm](https://hex.pm) dependencies in Elixir `mix.exs` files, inspired by the [vscode-hex-pm-intellisense](https://github.com/mrmckeb/vscode-hex-pm-intellisense) extension for VSCode.

## Features

- **Package name completions** — start typing a package name after `{:` and get live suggestions from the hex.pm registry ranked by download popularity.
- **Version completions** — once you've named a package, open the version string and get all available release versions in three convenient formats:
  - `~> X.Y` — semver-compatible constraint (one per minor series, pointing at the latest patch)
  - `>= X.Y.Z` — minimum-version constraint
  - `X.Y.Z` — exact pin
- **Caching** — API results are cached for 5 minutes so repeated completions are instant and don't hammer the network.
- **Zero noise in non-`mix.exs` files** — the language server silently ignores every Elixir file that isn't `mix.exs`.

## Demo

```elixir
defp deps do
  [
    # Type `{:ph` and trigger completions → phoenix, phoenix_html, …
    {:phoenix, "~> 1.7"},

    # Type `{:phoenix, "` and trigger completions → ~> 1.8, ~> 1.7, >= 1.8.5, …
    {:ecto_sql, "~> 3.12"},
  ]
end
```

## Installation

### 1. Install the Zed extension

Install **Hex.pm IntelliSense** from the Zed Extensions panel (`zed: extensions`).

For local development, open the Extensions panel, click **Install Dev Extension**,
and point it at the root of this repository.

> **No Rust toolchain required for end users.**  The extension automatically
> downloads the correct prebuilt `hex-ls` binary for your platform from the
> [GitHub Releases](https://github.com/timothyvanderaerden/zed-hex-intellisense/releases)
> page on first use and caches it in Zed's extension working directory.
> Subsequent launches reuse the cached binary; a new download only happens
> when a newer release is available.

#### Using a self-built binary instead

If you prefer to run your own build of `hex-ls` — or if you develop the
language server locally — place the binary anywhere on your `PATH` and the
extension will use it automatically (PATH lookup always takes priority over the
downloaded binary):

```sh
cargo install --path hex-ls
```

### 2. Enable `hex-ls` for Elixir in Zed settings

Zed only starts the language servers explicitly listed for a language. Add
`hex-ls` to the Elixir entry in `~/.config/zed/settings.json`:

```json
"languages": {
  "Elixir": {
    "language_servers": ["expert", "hex-ls"]
  }
}
```

If you already have a custom `language_servers` list for Elixir (e.g. with
disabled servers prefixed by `!`), simply append `"hex-ls"` to it.

### 4. Trigger completions

Open a `mix.exs` file and press `Ctrl+Space` (or your configured completion
shortcut) with the cursor:

- after `{:` to get package name suggestions
- inside the version string (e.g. `"~> "`) to get version suggestions

## Project structure

```
hex-intellisense/
├── extension.toml      ← Zed extension manifest
├── Cargo.toml          ← Workspace + extension WASM crate
├── Cargo.lock          ← Committed for reproducible hex-ls installs
├── src/
│   └── lib.rs          ← Extension WASM entry point
└── hex-ls/
    ├── Cargo.toml
    └── src/
        └── main.rs     ← LSP server binary
```

### Component overview

| Component | Role |
|---|---|
| `src/lib.rs` | Extension WASM — tells Zed where to find `hex-ls` |
| `hex-ls/src/main.rs` | Standalone LSP server — handles completions, queries hex.pm API |

## Development

Run the unit tests for the language server and the WASM extension:

```sh
cargo test -p hex-ls          # language server logic
cargo test -p hex-intellisense # asset-path construction tests
```

During local development of `hex-ls`, build and install the binary to avoid
waiting for a GitHub Release download:

```sh
cargo install --path hex-ls
```

The extension detects the `hex-ls` binary on `PATH` and skips the download
entirely — the same fast-path used by regular users who install from a release.

After modifying `src/lib.rs`, Zed will recompile the WASM extension
automatically when you reinstall the dev extension.

### Cutting a release

1. Bump the version in `Cargo.toml` (root), `hex-ls/Cargo.toml`, `extension.toml`,
   and the `USER_AGENT` constant in `hex-ls/src/main.rs`.
2. Update `CHANGELOG.md`.
3. Push a bare semver tag (no `v` prefix): `git tag 0.4.0 && git push --tags`.
4. The [Release workflow](.github/workflows/release.yml) builds `hex-ls` for
   all platforms and uploads the archives to a GitHub Release automatically.

## License

MIT
