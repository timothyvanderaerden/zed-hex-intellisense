use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
    HoverContents, HoverParams, HoverProviderCapability, MarkupContent, MarkupKind,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use regex::Regex;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const HEX_API_BASE: &str = "https://hex.pm/api";
const USER_AGENT: &str = "hex-ls/0.2.0";

// ---------------------------------------------------------------------------
// Lazy-compiled regexes
// ---------------------------------------------------------------------------

static VERSION_RE: OnceLock<Regex> = OnceLock::new();
static PKG_RE: OnceLock<Regex> = OnceLock::new();
static HOVER_PKG_RE: OnceLock<Regex> = OnceLock::new();
static VERSION_HOVER_RE: OnceLock<Regex> = OnceLock::new();

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

/// Matches `{:name` anywhere in the line (not end-anchored).
/// Captures: (1) package name
fn hover_pkg_re() -> &'static Regex {
    HOVER_PKG_RE.get_or_init(|| Regex::new(r"\{:\s*([a-z_][a-z_0-9]*)").unwrap())
}

/// Matches a complete dep tuple's version string, e.g. `{:phoenix, "~> 1.7"}`.
/// Captures: (1) package name, (2) version string content (between quotes).
fn version_hover_re() -> &'static Regex {
    // Matches: {:pkg_name, "version_string"}
    // Group 1 = package name, Group 2 = version string content (between quotes).
    VERSION_HOVER_RE.get_or_init(|| Regex::new(r#"\{:([a-z_][a-z_0-9]*)\s*,\s*"([^"]*)""#).unwrap())
}

struct ServerState {
    /// URI → full document text (updated on every didChange)
    documents: HashMap<String, String>,
    /// URL → (timestamp, parsed JSON) – simple TTL cache
    cache: Arc<Mutex<HashMap<String, (Instant, Value)>>>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            documents: HashMap::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ------------------------------------------------------------------
    // Cache helpers
    // ------------------------------------------------------------------

    fn get_cached(&self, url: &str) -> Option<Value> {
        let cache = self.cache.lock().unwrap();
        if let Some((ts, val)) = cache.get(url) {
            if ts.elapsed() < CACHE_TTL {
                return Some(val.clone());
            }
        }
        None
    }

    fn insert_cache(&self, url: String, val: Value) {
        self.cache
            .lock()
            .unwrap()
            .insert(url, (Instant::now(), val));
    }

    // ------------------------------------------------------------------
    // HTTP
    // ------------------------------------------------------------------

    /// Fetch `url` as JSON, consulting the cache first.
    /// Returns `None` on any error (logs to stderr).
    fn fetch_json(&mut self, url: &str) -> Option<Value> {
        // Cache hit
        if let Some(cached) = self.get_cached(url) {
            return Some(cached);
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

    /// Spawn a background thread to pre-warm the cache for all packages
    /// found in `text`. This is called on `textDocument/didOpen` so that
    /// hover requests are served instantly from cache.
    fn prefetch_packages(&self, text: &str) {
        let re = hover_pkg_re();
        let mut seen = std::collections::HashSet::new();
        let packages: Vec<String> = re
            .captures_iter(text)
            .filter_map(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())
            .filter(|pkg| seen.insert(pkg.clone()))
            .take(30) // cap at 30 to avoid hammering the API
            .collect();

        if packages.is_empty() {
            return;
        }

        eprintln!("hex-ls: prefetching {} package(s)", packages.len());
        let cache = Arc::clone(&self.cache);
        std::thread::spawn(move || {
            for pkg in packages {
                let url = format!("{HEX_API_BASE}/packages/{pkg}");
                // Skip if already cached and fresh
                {
                    let c = cache.lock().unwrap();
                    if let Some((ts, _)) = c.get(&url) {
                        if ts.elapsed() < CACHE_TTL {
                            continue;
                        }
                    }
                }
                match ureq::get(&url).set("User-Agent", USER_AGENT).call() {
                    Ok(resp) => {
                        if let Ok(val) = resp.into_json::<Value>() {
                            cache.lock().unwrap().insert(url, (Instant::now(), val));
                        }
                    }
                    Err(e) => {
                        eprintln!("hex-ls: prefetch error for {pkg}: {e}");
                    }
                }
            }
            eprintln!("hex-ls: prefetch complete");
        });
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
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
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
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    };

    // Perform the LSP handshake (initialize request / response)
    let (init_id, _init_params) = connection.initialize_start().unwrap();
    let init_result = serde_json::json!({
        "capabilities": server_capabilities,
        "serverInfo": {
            "name": "hex-ls",
            "version": "0.2.0"
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

        "textDocument/hover" => {
            let params: HoverParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("hex-ls: bad hover params: {e}");
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

            let result = compute_hover(state, &params);
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
            if let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(notif.params) {
                let uri = params.text_document.uri.to_string();
                eprintln!("hex-ls: opened {uri}");
                if uri.contains("mix.exs") {
                    state.prefetch_packages(&params.text_document.text);
                }
                state.documents.insert(uri, params.text_document.text);
            }
        }

        "textDocument/didChange" => {
            if let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(notif.params)
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
            if let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(notif.params) {
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
    let uri = params.text_document_position.text_document.uri.to_string();

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
        Some(CompletionContext::PackageName(partial)) => complete_package_names(state, &partial),
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
// Hover support
// ---------------------------------------------------------------------------

/// Return the package name if the cursor sits inside the name atom of a dep
/// tuple like `{:phoenix, "~> 1.7"}`.  Uses a non-`?` guard so that a
/// missing capture group skips the match rather than short-circuiting.
fn package_name_at_cursor(line: &str, char_idx: usize) -> Option<String> {
    let re = hover_pkg_re();
    for caps in re.captures_iter(line) {
        if let Some(m) = caps.get(1) {
            // m.end() is exclusive, so use < (not <=) to avoid matching
            // the character immediately after the package name.
            if char_idx >= m.start() && char_idx < m.end() {
                return Some(m.as_str().to_string());
            }
        }
    }
    None
}

/// Return the package name if the cursor sits inside the quoted version string
/// of a dep tuple, e.g. `{:phoenix, "~> 1.7"}`.  Returns `None` otherwise.
fn version_pkg_at_cursor(line: &str, char_idx: usize) -> Option<String> {
    let re = version_hover_re();
    for caps in re.captures_iter(line) {
        if let (Some(pkg_m), Some(ver_m)) = (caps.get(1), caps.get(2)) {
            // The opening quote is the byte immediately before the captured
            // content; the closing quote sits at ver_m.end().
            let quote_open = ver_m.start().saturating_sub(1);
            let quote_close = ver_m.end(); // inclusive (closing `"`)
            if char_idx >= quote_open && char_idx <= quote_close {
                return Some(pkg_m.as_str().to_string());
            }
        }
    }
    None
}

fn compute_hover(state: &mut ServerState, params: &HoverParams) -> Option<Hover> {
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .to_string();

    // Only provide hover inside mix.exs files.
    if !uri.contains("mix.exs") {
        return None;
    }

    // Retrieve the stored document text.
    let content = state.documents.get(&uri)?.clone();

    let line_idx = params.text_document_position_params.position.line as usize;
    let char_idx = params.text_document_position_params.position.character as usize;

    // Split into lines, handling \r\n.
    let lines: Vec<&str> = content.lines().collect();
    let current_line = lines.get(line_idx)?.trim_end_matches('\r');

    // Safe byte-offset (same logic as compute_completions).
    let safe_idx = char_idx.min(current_line.len());
    let safe_idx = (0..=safe_idx)
        .rev()
        .find(|&i| current_line.is_char_boundary(i))
        .unwrap_or(0);

    // Hover over the package name → show package info.
    if let Some(pkg_name) = package_name_at_cursor(current_line, safe_idx) {
        return hover_package_info(state, &pkg_name);
    }

    // Hover over the version string → show available versions.
    if let Some(pkg_name) = version_pkg_at_cursor(current_line, safe_idx) {
        return hover_version_list(state, &pkg_name);
    }

    None
}

/// Build a hover showing description, latest version, license, and links.
fn hover_package_info(state: &mut ServerState, pkg_name: &str) -> Option<Hover> {
    let url = format!("{HEX_API_BASE}/packages/{pkg_name}");
    let data = state.fetch_json(&url)?;

    // Extract fields.
    let version = data["latest_stable_version"]
        .as_str()
        .or_else(|| data["latest_version"].as_str())
        .unwrap_or("unknown");

    let description = data["meta"]["description"].as_str().unwrap_or("");

    let licenses: Vec<&str> = data["meta"]["licenses"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let html_url = data["html_url"].as_str().unwrap_or("");
    let docs_url = data["docs_html_url"].as_str().unwrap_or("");

    // Build the markdown response.
    let mut md = format!("### {pkg_name} `{version}`\n\n");

    if !description.is_empty() {
        md.push_str(description);
        md.push_str("\n\n");
    }

    if !licenses.is_empty() {
        md.push_str(&format!("**License:** {}\n\n", licenses.join(", ")));
    }

    // Build links line.
    let mut links: Vec<String> = Vec::new();
    if !docs_url.is_empty() {
        links.push(format!("[HexDocs]({docs_url})"));
    }
    if !html_url.is_empty() {
        links.push(format!("[Hex.pm]({html_url})"));
    }
    // Additional links from meta.links object.
    if let Some(meta_links) = data["meta"]["links"].as_object() {
        for (key, val) in meta_links {
            if let Some(href) = val.as_str() {
                links.push(format!("[{key}]({href})"));
            }
        }
    }
    if !links.is_empty() {
        md.push_str(&links.join(" \u{00b7} "));
        md.push('\n');
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    })
}

/// Build a hover showing all available versions grouped by minor.
fn hover_version_list(state: &mut ServerState, pkg_name: &str) -> Option<Hover> {
    let url = format!("{HEX_API_BASE}/packages/{pkg_name}");
    let data = state.fetch_json(&url)?;

    let releases = data["releases"].as_array()?.clone();

    // Group versions by their X.Y minor key, preserving newest-first order.
    let mut minors: Vec<String> = Vec::new();
    let mut by_minor: HashMap<String, Vec<String>> = HashMap::new();

    for r in releases.iter().take(60) {
        if let Some(ver) = r["version"].as_str() {
            if let Some(mk) = minor_key(ver) {
                if !by_minor.contains_key(&mk) {
                    minors.push(mk.clone());
                }
                by_minor.entry(mk).or_default().push(ver.to_string());
            }
        }
    }

    if minors.is_empty() {
        return None;
    }

    let mut md = format!("### {} — available versions\n\n", pkg_name);
    let display_minors = minors.iter().take(8).collect::<Vec<_>>();
    for mk in &display_minors {
        let versions = &by_minor[*mk];
        let shown: Vec<&str> = versions.iter().take(5).map(String::as_str).collect();
        let ellipsis = if versions.len() > 5 { " …" } else { "" };
        md.push_str(&format!(
            "**~> {}** &nbsp; {}{}\n\n",
            mk,
            shown.join(" · "),
            ellipsis
        ));
    }
    if minors.len() > 8 {
        md.push_str(&format!(
            "_{} more minor version(s) — see [Hex.pm]({})_\n",
            minors.len() - 8,
            data["html_url"].as_str().unwrap_or("https://hex.pm")
        ));
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    })
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

    // -----------------------------------------------------------------------
    // Hover position detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_package_name_at_cursor_on_name() {
        // {:phoenix, "~> 1.7"}
        // 0123456789...
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // cursor on 'h' (byte 2)
        assert_eq!(package_name_at_cursor(line, 2), Some("phoenix".into()));
        // cursor on last 'x' (byte 8)
        assert_eq!(package_name_at_cursor(line, 8), Some("phoenix".into()));
    }

    #[test]
    fn test_package_name_at_cursor_with_leading_spaces() {
        let line = r#"      {:ecto, "~> 3.0"}"#;
        //                 ^^^^ name starts at byte 8
        assert_eq!(package_name_at_cursor(line, 8), Some("ecto".into()));
        assert_eq!(package_name_at_cursor(line, 10), Some("ecto".into()));
    }

    #[test]
    fn test_package_name_at_cursor_miss() {
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // cursor is on the comma (byte 9) - outside the name range
        assert_eq!(package_name_at_cursor(line, 9), None);
        // cursor on 'p' of "project" in an unrelated line
        assert_eq!(package_name_at_cursor("def project do", 4), None);
    }

    #[test]
    fn test_version_pkg_at_cursor_on_quote_open() {
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // Opening '"' is at byte 11, content starts at 12
        assert_eq!(version_pkg_at_cursor(line, 11), Some("phoenix".into()));
    }

    #[test]
    fn test_version_pkg_at_cursor_inside_version() {
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // Inside '~> 1.7' - e.g. byte 14 (space after '>')
        assert_eq!(version_pkg_at_cursor(line, 14), Some("phoenix".into()));
    }

    #[test]
    fn test_version_pkg_at_cursor_on_quote_close() {
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // Closing '"' is at byte 18
        assert_eq!(version_pkg_at_cursor(line, 18), Some("phoenix".into()));
    }

    #[test]
    fn test_version_pkg_at_cursor_miss() {
        let line = r#"{:phoenix, "~> 1.7"}"#;
        // Cursor before the opening quote (byte 10, which is the space)
        assert_eq!(version_pkg_at_cursor(line, 10), None);
        // Cursor after the closing quote (byte 19, '}')
        assert_eq!(version_pkg_at_cursor(line, 19), None);
    }
}
