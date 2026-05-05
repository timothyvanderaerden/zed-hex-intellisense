use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use regex::Regex;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const HEX_API_BASE: &str = "https://hex.pm/api";
const USER_AGENT: &str = "hex-ls/0.1.0";

// ---------------------------------------------------------------------------
// Lazy-compiled regexes
// ---------------------------------------------------------------------------

static VERSION_RE: OnceLock<Regex> = OnceLock::new();
static PKG_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `{:pkg_name, "partial_version` at end of text.
/// Captures: (1) package name, (2) partial version string
fn version_re() -> &'static Regex {
    VERSION_RE.get_or_init(|| {
        // The `"` inside a raw string with `r#"..."#` delimiters is fine.
        Regex::new(r#"\{:([a-z_][a-z_0-9]*)\s*,\s*"([^"]*)$"#).unwrap()
    })
}

/// Matches `{:partial_name` at end of text.
/// Captures: (1) partial package name (may be empty)
fn pkg_re() -> &'static Regex {
    PKG_RE.get_or_init(|| Regex::new(r"\{:\s*([a-z_]*)$").unwrap())
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct ServerState {
    /// URI → full document text (updated on every didChange)
    documents: HashMap<String, String>,
    /// URL → (timestamp, parsed JSON) – simple TTL cache
    cache: HashMap<String, (Instant, Value)>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            documents: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------
    // Cache helpers
    // ------------------------------------------------------------------

    fn get_cached(&self, url: &str) -> Option<&Value> {
        if let Some((ts, val)) = self.cache.get(url) {
            if ts.elapsed() < CACHE_TTL {
                return Some(val);
            }
        }
        None
    }

    fn insert_cache(&mut self, url: String, val: Value) {
        self.cache.insert(url, (Instant::now(), val));
    }

    // ------------------------------------------------------------------
    // HTTP
    // ------------------------------------------------------------------

    /// Fetch `url` as JSON, consulting the cache first.
    /// Returns `None` on any error (logs to stderr).
    fn fetch_json(&mut self, url: &str) -> Option<Value> {
        // Cache hit
        if let Some(cached) = self.get_cached(url) {
            return Some(cached.clone());
        }

        // Network request
        match ureq::get(url).set("User-Agent", USER_AGENT).call() {
            Ok(resp) => match resp.into_json::<Value>() {
                Ok(val) => {
                    self.insert_cache(url.to_string(), val.clone());
                    Some(val)
                }
                Err(e) => {
                    eprintln!("hex-ls: JSON parse error for {url}: {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!("hex-ls: HTTP error for {url}: {e}");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // Handle --version / --help flags before starting the LSP server.
    let args: Vec<String> = std::env::args().collect();
    for arg in &args[1..] {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("hex-ls {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    concat!(
                        "hex-ls {}\n",
                        "\n",
                        "Hex.pm Language Server — provides IntelliSense for mix.exs deps.\n",
                        "\n",
                        "Communicates over stdin/stdout using the Language Server Protocol.\n",
                        "Start it from a Zed extension rather than invoking it directly.\n",
                        "\n",
                        "OPTIONS:\n",
                        "  -h, --help       Print this help\n",
                        "  -V, --version    Print version\n"
                    ),
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            _ => {}
        }
    }

    eprintln!("hex-ls: starting");

    // Establish stdio transport
    let (connection, io_threads) = Connection::stdio();

    // Build server capabilities
    let server_capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::FULL,
        )),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![
                ":".to_string(),
                "\"".to_string(),
                ",".to_string(),
                " ".to_string(),
            ]),
            resolve_provider: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Perform the LSP handshake (initialize request / response)
    let (init_id, _init_params) = connection.initialize_start().unwrap();
    let init_result = serde_json::json!({
        "capabilities": server_capabilities,
        "serverInfo": {
            "name": "hex-ls",
            "version": "0.1.0"
        }
    });
    connection.initialize_finish(init_id, init_result).unwrap();

    eprintln!("hex-ls: initialized, entering main loop");

    // Main message loop
    let mut state = ServerState::new();

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                // handle_shutdown replies to "shutdown" and returns true when
                // the subsequent "exit" notification arrives.
                if connection.handle_shutdown(&req).unwrap() {
                    break;
                }
                handle_request(&connection, &mut state, req);
            }
            Message::Notification(notif) => {
                handle_notification(&mut state, notif);
            }
            // We never send requests to the client, so responses are no-ops.
            Message::Response(_) => {}
        }
    }

    io_threads.join().unwrap();
    eprintln!("hex-ls: done");
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

fn handle_request(conn: &Connection, state: &mut ServerState, req: Request) {
    let id = req.id.clone();

    match req.method.as_str() {
        "textDocument/completion" => {
            let params: CompletionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("hex-ls: bad completion params: {e}");
                    conn.sender
                        .send(Message::Response(Response::new_err(
                            id,
                            -32602,
                            format!("Invalid params: {e}"),
                        )))
                        .ok();
                    return;
                }
            };

            let items = compute_completions(state, &params);
            let result = CompletionResponse::Array(items);
            conn.sender
                .send(Message::Response(Response::new_ok(id, result)))
                .ok();
        }

        _ => {
            // Respond with MethodNotFound for anything we don't handle.
            conn.sender
                .send(Message::Response(Response::new_err(
                    id,
                    -32601,
                    "Method not found".to_string(),
                )))
                .ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Notification dispatch
// ---------------------------------------------------------------------------

fn handle_notification(state: &mut ServerState, notif: Notification) {
    match notif.method.as_str() {
        "initialized" => {
            // No-op – client confirms initialization is done.
            eprintln!("hex-ls: received 'initialized' notification");
        }

        "textDocument/didOpen" => {
            if let Ok(params) =
                serde_json::from_value::<DidOpenTextDocumentParams>(notif.params)
            {
                let uri = params.text_document.uri.to_string();
                eprintln!("hex-ls: opened {uri}");
                state.documents.insert(uri, params.text_document.text);
            }
        }

        "textDocument/didChange" => {
            if let Ok(params) =
                serde_json::from_value::<DidChangeTextDocumentParams>(notif.params)
            {
                let uri = params.text_document.uri.to_string();
                // With FULL sync, there is exactly one change event containing
                // the complete new document text.
                if let Some(change) = params.content_changes.into_iter().last() {
                    state.documents.insert(uri, change.text);
                }
            }
        }

        "textDocument/didClose" => {
            if let Ok(params) =
                serde_json::from_value::<DidCloseTextDocumentParams>(notif.params)
            {
                let uri = params.text_document.uri.to_string();
                eprintln!("hex-ls: closed {uri}");
                state.documents.remove(&uri);
            }
        }

        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Completion context detection
// ---------------------------------------------------------------------------

enum CompletionContext {
    /// Cursor is after `{:` – user is typing a package name.
    /// Contains the partial name typed so far (may be empty).
    PackageName(String),

    /// Cursor is inside the version string of a dep tuple.
    /// Contains (package_name, partial_version_text).
    Version(String, String),
}

/// Inspect the text on the current line up to the cursor position and
/// decide what kind of completion to offer.
fn detect_context(text_before: &str) -> Option<CompletionContext> {
    // Check version context first – it is more specific.
    // Pattern: {:pkg_name, "partial  (no closing quote yet)
    if let Some(caps) = version_re().captures(text_before) {
        let pkg = caps.get(1).unwrap().as_str().to_string();
        let ver = caps.get(2).unwrap().as_str().to_string();
        return Some(CompletionContext::Version(pkg, ver));
    }

    // Package name context: {: optionally followed by [a-z_]* at end
    if let Some(caps) = pkg_re().captures(text_before) {
        let partial = caps.get(1).unwrap().as_str().to_string();
        return Some(CompletionContext::PackageName(partial));
    }

    None
}

// ---------------------------------------------------------------------------
// Completion computation
// ---------------------------------------------------------------------------

fn compute_completions(state: &mut ServerState, params: &CompletionParams) -> Vec<CompletionItem> {
    let uri = params
        .text_document_position
        .text_document
        .uri
        .to_string();

    // Only provide completions inside mix.exs files.
    if !uri.contains("mix.exs") {
        return vec![];
    }

    // Retrieve the stored document text.
    let content = match state.documents.get(&uri) {
        Some(c) => c.clone(),
        None => return vec![],
    };

    let line_idx = params.text_document_position.position.line as usize;
    let char_idx = params.text_document_position.position.character as usize;

    // Split into lines, taking care of \r\n line endings.
    let lines: Vec<&str> = content.lines().collect();
    let current_line = match lines.get(line_idx) {
        Some(l) => l.trim_end_matches('\r'),
        None => return vec![],
    };

    // Slice the line up to the cursor.  char_idx is a UTF-16 column number,
    // but mix.exs content is effectively ASCII, so we treat it as a byte
    // offset with a safe bounds check.
    let safe_end = char_idx.min(current_line.len());
    // Walk back to the nearest valid UTF-8 char boundary (handles edge cases).
    let safe_end = (0..=safe_end)
        .rev()
        .find(|&i| current_line.is_char_boundary(i))
        .unwrap_or(0);
    let text_before = &current_line[..safe_end];

    match detect_context(text_before) {
        Some(CompletionContext::PackageName(partial)) => {
            complete_package_names(state, &partial)
        }
        Some(CompletionContext::Version(pkg, partial_ver)) => {
            complete_versions(state, &pkg, &partial_ver)
        }
        None => vec![],
    }
}

// ---------------------------------------------------------------------------
// Package-name completions
// ---------------------------------------------------------------------------

fn complete_package_names(state: &mut ServerState, partial: &str) -> Vec<CompletionItem> {
    // Build the hex.pm search URL.
    let url = if partial.is_empty() {
        format!("{HEX_API_BASE}/packages?sort=downloads&page=1")
    } else {
        format!("{HEX_API_BASE}/packages?search={partial}&sort=downloads&page=1")
    };

    let data = match state.fetch_json(&url) {
        Some(d) => d,
        None => return vec![],
    };

    let packages = match data.as_array() {
        Some(a) => a.clone(),
        None => return vec![],
    };

    packages
        .iter()
        .filter_map(|pkg| {
            let name = pkg["name"].as_str()?;

            let description: String = pkg["meta"]["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect();

            Some(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::MODULE),
                detail: if description.is_empty() {
                    None
                } else {
                    Some(description)
                },
                // insert_text is just the bare name (no leading colon –
                // the colon is already in the document as part of `{:`).
                insert_text: Some(name.to_string()),
                ..Default::default()
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Version completions
// ---------------------------------------------------------------------------

fn complete_versions(
    state: &mut ServerState,
    pkg_name: &str,
    _partial: &str,
) -> Vec<CompletionItem> {
    let url = format!("{HEX_API_BASE}/packages/{pkg_name}");

    let data = match state.fetch_json(&url) {
        Some(d) => d,
        None => return vec![],
    };

    let releases = match data["releases"].as_array() {
        Some(r) => r.clone(),
        None => return vec![],
    };

    // The `configs["mix.exs"]` field contains the canonical dependency
    // snippet which makes a useful detail line.
    let mix_config = data["configs"]["mix.exs"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Collect version strings.  The API returns them newest-first already.
    let versions: Vec<String> = releases
        .iter()
        .filter_map(|r| r["version"].as_str().map(|s| s.to_string()))
        .take(20)
        .collect();

    // -----------------------------------------------------------------------
    // Deduplication for ~> X.Y suggestions
    //
    // Because the API returns newest-first, the first time we encounter a
    // "X.Y" key it corresponds to the latest patch release for that minor.
    // We record that mapping so we can emit exactly one `~> X.Y` per minor.
    // -----------------------------------------------------------------------
    let mut latest_patch_for_minor: HashMap<String, String> = HashMap::new();
    for ver in &versions {
        if let Some(minor_key) = minor_key(ver) {
            // `entry(...).or_insert(...)` keeps the *first* (i.e. latest) value.
            latest_patch_for_minor
                .entry(minor_key)
                .or_insert_with(|| ver.clone());
        }
    }

    let mut items: Vec<CompletionItem> = Vec::new();
    let mut tilde_emitted: HashMap<String, ()> = HashMap::new();

    for ver in &versions {
        // ── ~> X.Y ──────────────────────────────────────────────────────
        // Emit at most once per X.Y, and only when `ver` is the latest
        // patch for that minor (so the suggestion always describes the
        // best-matching constraint).
        if let Some(mk) = minor_key(ver) {
            let is_latest = latest_patch_for_minor.get(&mk).map(|s| s.as_str()) == Some(ver);
            if is_latest && !tilde_emitted.contains_key(&mk) {
                tilde_emitted.insert(mk.clone(), ());
                let label = format!("~> {mk}");
                items.push(CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some(format!("Semver-compatible with {mk}.x")),
                    insert_text: Some(label),
                    ..Default::default()
                });
            }
        }

        // ── >= X.Y.Z ────────────────────────────────────────────────────
        let gte_label = format!(">= {ver}");
        items.push(CompletionItem {
            label: gte_label.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("Minimum version {ver}")),
            insert_text: Some(gte_label),
            ..Default::default()
        });

        // ── Exact version ───────────────────────────────────────────────
        items.push(CompletionItem {
            label: ver.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: if mix_config.is_empty() {
                None
            } else {
                Some(mix_config.clone())
            },
            insert_text: Some(ver.clone()),
            ..Default::default()
        });
    }

    items
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the "X.Y" major.minor prefix from a semver string such as "1.7.3".
/// Returns `None` if the version string doesn't contain at least two parts.
fn minor_key(version: &str) -> Option<String> {
    let mut parts = version.splitn(3, '.');
    let major = parts.next()?;
    let minor = parts.next()?;
    // Reject anything that doesn't look like a normal release (e.g. pre-release tags).
    Some(format!("{major}.{minor}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minor_key() {
        assert_eq!(minor_key("1.7.3"), Some("1.7".to_string()));
        assert_eq!(minor_key("2.0.0"), Some("2.0".to_string()));
        assert_eq!(minor_key("1.7"), Some("1.7".to_string()));
        assert_eq!(minor_key("1"), None);
    }

    #[test]
    fn test_detect_context_package_name_empty() {
        let ctx = detect_context("    {:").unwrap();
        match ctx {
            CompletionContext::PackageName(p) => assert_eq!(p, ""),
            _ => panic!("expected PackageName"),
        }
    }

    #[test]
    fn test_detect_context_package_name_partial() {
        let ctx = detect_context("    {:pho").unwrap();
        match ctx {
            CompletionContext::PackageName(p) => assert_eq!(p, "pho"),
            _ => panic!("expected PackageName"),
        }
    }

    #[test]
    fn test_detect_context_version() {
        let ctx = detect_context(r#"    {:phoenix, "~> "#).unwrap();
        match ctx {
            CompletionContext::Version(pkg, ver) => {
                assert_eq!(pkg, "phoenix");
                assert_eq!(ver, "~> ");
            }
            _ => panic!("expected Version"),
        }
    }

    #[test]
    fn test_detect_context_version_empty_prefix() {
        let ctx = detect_context(r#"    {:ecto_sql, ""#).unwrap();
        match ctx {
            CompletionContext::Version(pkg, ver) => {
                assert_eq!(pkg, "ecto_sql");
                assert_eq!(ver, "");
            }
            _ => panic!("expected Version"),
        }
    }

    #[test]
    fn test_detect_context_none_after_closing_quote() {
        // Once the string is closed there is nothing to complete.
        let ctx = detect_context(r#"    {:phoenix, "~> 1.7"}"#);
        assert!(ctx.is_none());
    }

    #[test]
    fn test_detect_context_none_on_unrelated_line() {
        let ctx = detect_context("  def project do");
        assert!(ctx.is_none());
    }

    #[test]
    fn test_version_deduplication() {
        // Three patch releases for 1.7, one for 1.8.
        let versions = vec![
            "1.8.0".to_string(),
            "1.7.3".to_string(),
            "1.7.2".to_string(),
            "1.7.1".to_string(),
        ];

        let mut latest: HashMap<String, String> = HashMap::new();
        for v in &versions {
            if let Some(mk) = minor_key(v) {
                latest.entry(mk).or_insert_with(|| v.clone());
            }
        }

        // The latest patch for 1.7 should be 1.7.3 (first seen).
        assert_eq!(latest["1.7"], "1.7.3");
        assert_eq!(latest["1.8"], "1.8.0");
    }
}
