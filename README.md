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

### Prerequisites

Zed compiles extensions to WebAssembly and requires [`rustup`](https://rustup.rs)
to manage the `wasm32-wasip2` target. Make sure it is installed and the target
is available before installing the extension.

### 1. Install the `hex-ls` language server

The extension delegates all completion logic to the `hex-ls` binary, which must
be available in your `PATH`. From the root of this repository:

```sh
cargo install --path hex-ls
```

Verify it works:

```sh
hex-ls --version
```

### 2. Install the Zed extension

In Zed, open the Extensions panel (`zed: extensions`), click
**Install Dev Extension**, and point it at the root of this repository.

### 3. Enable `hex-ls` for Elixir in Zed settings

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

Run the unit tests for the language server:

```sh
cargo test -p hex-ls
```

After modifying `hex-ls/src/main.rs`, reinstall the binary and restart Zed
(or reload the window):

```sh
cargo install --path hex-ls
```

After modifying `src/lib.rs`, Zed will recompile the WASM extension
automatically when you reinstall the dev extension.

## License

MIT
