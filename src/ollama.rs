use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One monitored subscription (an Ollama Cloud API key).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyEntry {
    pub label: String,
    pub key: String,
}

/// Usage data for one key, as returned by ollama.com.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    /// HTTP/network error, if any.
    pub error: Option<String>,
    /// usage fraction per window, canonically ordered: Monthly, Weekly, Session, …
    pub windows: Vec<(String, f64)>,
    /// per-model request counts (from the primary window)
    pub models: Vec<(String, u64)>,
    /// which window `models` came from, e.g. "Weekly"
    pub models_window: Option<String>,
    /// activity period description, e.g. "last 4 weeks (since 2026-08-31)"
    pub period: Option<String>,
}

/// Capitalize a window name from the raw API ("weekly" → "Weekly").
fn cap(s: &str) -> String {
    let mut c = s.to_string();
    if !c.is_empty() {
        let first = c.remove(0).to_uppercase().to_string();
        c.insert_str(0, &first);
    }
    c
}

/// Canonical window ordering: Monthly, Weekly, Session, then anything else.
fn rank(name: &str) -> (u8, String) {
    match name {
        "Monthly" => (0, String::new()),
        "Weekly" => (1, String::new()),
        "Session" => (2, String::new()),
        other => (3, other.to_string()),
    }
}

/// Defensive extraction of a 0..1 usage fraction from a window object.
/// Handles `usage` (0-1), `used`/`used_percentage` (0-1 or 0-100).
fn window_usage(w: &serde_json::Value) -> Option<f64> {
    let norm = |mut v: f64| {
        if v > 1.0 {
            v /= 100.0;
        }
        v.clamp(0.0, 1.0)
    };
    if let Some(v) = w.get("usage").and_then(|v| v.as_f64()) {
        return Some(norm(v));
    }
    if let Some(v) = w.get("used_percentage").and_then(|v| v.as_f64()) {
        return Some(norm(v));
    }
    if let Some(v) = w.get("used").and_then(|v| v.as_f64()) {
        return Some(norm(v));
    }
    None
}

fn window_models(w: &serde_json::Value) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Some(arr) = w.get("models").and_then(|m| m.as_array()) {
        for m in arr {
            let name = m
                .get("name")
                .or_else(|| m.get("model"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let n = m
                .get("request_count")
                .or_else(|| m.get("requests"))
                .or_else(|| m.get("count"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            out.push((name, n));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

fn parse_usage(
    v: &serde_json::Value,
) -> (
    Vec<(String, f64)>,
    Vec<(String, u64)>,
    Option<String>,
    Option<String>,
) {
    let mut windows: Vec<(String, f64)> = Vec::new();
    let mut models_by_window: Vec<(String, Vec<(String, u64)>)> = Vec::new();

    if let Some(limits) = v.get("limits").and_then(|l| l.as_object()) {
        for (name, w) in limits {
            if let Some(u) = window_usage(w) {
                let mut label = name.clone();
                label[..1].make_ascii_uppercase();
                windows.push((label, u));
                models_by_window.push((name.clone(), window_models(w)));
            }
        }
    }
    windows.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)));

    // models: prefer Weekly (richest), then Session, then any window with data,
    // then fall back to activity.models (last 4 weeks).
    let order: HashMap<String, usize> = models_by_window
        .iter()
        .enumerate()
        .map(|(i, (n, _))| (n.clone(), i))
        .collect();
    let mut models: Vec<(String, u64)> = Vec::new();
    let mut models_window: Option<String> = None;
    for pref in ["weekly", "session", "monthly"] {
        if let Some(i) = order.get(pref) {
            let (wname, m) = &models_by_window[*i];
            if !m.is_empty() {
                models = m.clone();
                models_window = Some(cap(wname));
                break;
            }
        }
    }
    if models.is_empty() {
        for (wname, m) in &models_by_window {
            if !m.is_empty() {
                models = m.clone();
                models_window = Some(cap(wname));
                break;
            }
        }
    }
    // fallback: activity.models (rolling last-4-weeks window)
    if models.is_empty() {
        if let Some(arr) = v.get("activity").and_then(|a| a.get("models")).and_then(|m| m.as_array())
        {
            for m in arr {
                let name = m
                    .get("name")
                    .or_else(|| m.get("model"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string();
                let n = m
                    .get("request_count")
                    .or_else(|| m.get("requests"))
                    .or_else(|| m.get("count"))
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0);
                models.push((name, n));
            }
            models.sort_by(|a, b| b.1.cmp(&a.1));
            models_window = Some("Last 4 weeks".into());
        }
    }
    // activity period: rolling window, described honestly (no fake countdown —
    // `ending_at` tracks the fetch moment, not a fixed reset point)
    let period = v
        .get("activity")
        .and_then(|a| a.get("period"))
        .and_then(|p| p.as_object())
        .map(|p| {
            let kind = p
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("rolling")
                .replace('_', " ");
            match p.get("starting_at").and_then(|s| s.as_str()) {
                Some(s) => format!("{kind} (since {})", &s[..10.min(s.len())]),
                None => kind,
            }
        });

    (windows, models, models_window, period)
}

/// Fetch usage for one API key from the official endpoint.
pub fn fetch_usage(key: &str) -> Usage {
    let resp = ureq::get("https://ollama.com/api/usage")
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(std::time::Duration::from_secs(15))
        .call();

    match resp {
        Ok(r) => match r.into_json::<serde_json::Value>() {
            Ok(v) => {
                let (windows, models, models_window, period) = parse_usage(&v);
                if windows.is_empty() {
                    Usage {
                        error: Some("no usage windows in response".into()),
                        windows,
                        models,
                        models_window,
                        period,
                    }
                } else {
                    Usage { error: None, windows, models, models_window, period }
                }
            }
            Err(e) => Usage { error: Some(format!("bad JSON: {e}")), ..Default::default() },
        },
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let body = body.chars().take(120).collect::<String>();
            let hint = match code {
                401 => "invalid API key",
                429 => "rate limited",
                _ => "API error",
            };
            Usage { error: Some(format!("HTTP {code} {hint} · {body}")), ..Default::default() }
        }
        Err(e) => Usage { error: Some(format!("network: {e}")), ..Default::default() },
    }
}

/// Format a 0..1 usage fraction into (used%, left%).
pub fn pct(usage: f64) -> (f64, f64) {
    let used = (usage * 100.0).clamp(0.0, 100.0);
    (used, 100.0 - used)
}

/// Aggregate model request counts across a set of usages.
pub fn aggregate_models<'a, I>(usages: I) -> Vec<(String, u64)>
where
    I: Iterator<Item = &'a Usage>,
{
    let mut acc: HashMap<String, u64> = HashMap::new();
    for u in usages {
        for (m, n) in &u.models {
            *acc.entry(m.clone()).or_insert(0) += n;
        }
    }
    let mut v: Vec<(String, u64)> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

/// Union of window names across usages, canonically ordered.
pub fn all_windows<'a, I>(usages: I) -> Vec<String>
where
    I: Iterator<Item = &'a Usage>,
{
    let mut names: Vec<String> = Vec::new();
    for u in usages {
        for (w, _) in &u.windows {
            if !names.contains(w) {
                names.push(w.clone());
            }
        }
    }
    names.sort_by(|a, b| rank(a).cmp(&rank(b)));
    names
}

/// Store -------------------------------------------------------------------
///
/// Secrets (API keys) live in the OS keyring — Windows Credential Manager or
/// the freedesktop Secret Service on Linux. The JSON file stores only
/// non-secret metadata (labels); the file is also the fallback when no
/// keyring is available.
#[derive(Serialize, Deserialize, Default)]
pub struct KeyStore {
    pub keys: Vec<KeyEntry>,
}

const SERVICE: &str = "ollama-shepherd";

/// Read all keys: labels from the JSON file, secrets from the keyring.
/// Falls back to a `key` field in the JSON (legacy plaintext store) when the
/// keyring has no entry.
pub fn load_keys() -> (Vec<KeyEntry>, Option<String>) {
    let (mut store, warning) = load_store_with_backup();
    for (i, k) in store.keys.iter_mut().enumerate() {
        if k.key.is_empty() {
            match keyring::Entry::new(SERVICE, &format!("key-{i}")) {
                Ok(entry) => match entry.get_password() {
                    Ok(secret) => k.key = secret,
                    Err(_) => {} // stays empty; fetch will report the failure
                },
                Err(_) => {}
            }
        }
    }
    // legacy fallback: plaintext key fields from older versions
    if store.keys.iter().all(|k| k.key.is_empty()) {
        if let Some(path) = legacy_path() {
            if let Ok(txt) = std::fs::read_to_string(path) {
                if let Ok(legacy) = serde_json::from_str::<KeyStore>(&txt) {
                    for (i, k) in legacy.keys.iter().enumerate() {
                        if let Some(slot) = store.keys.get_mut(i) {
                            if slot.key.is_empty() {
                                slot.key = k.key.clone();
                                migrate_to_keyring(i, &k.key);
                            }
                        }
                    }
                    save_store(&store);
                }
            }
        }
    }
    (store.keys, warning)
}

fn migrate_to_keyring(index: usize, secret: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("key-{index}")) {
        let _ = entry.set_password(secret);
    }
}

fn legacy_path() -> Option<std::path::PathBuf> {
    home().map(|h| h.join(".ollama-shepherd").join("keys.json"))
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

/// Load store from disk; on corrupt JSON, move it aside and return a warning.
fn load_store_with_backup() -> (KeyStore, Option<String>) {
    let path = match legacy_path() {
        Some(p) => p,
        None => return (KeyStore::default(), None),
    };
    let txt = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (KeyStore::default(), None),
    };
    match serde_json::from_str::<KeyStore>(&txt) {
        Ok(store) => (store, None),
        Err(e) => {
            let bak = path.with_extension("json.bak");
            let note = if std::fs::rename(&path, &bak).is_ok() {
                format!(
                    "keys.json was corrupt (moved to {}) — re-add your keys. ({e})",
                    bak.display()
                )
            } else {
                format!("keys.json is corrupt and could not be backed up. ({e})")
            };
            (KeyStore::default(), Some(note))
        }
    }
}

fn save_store(store: &KeyStore) {
    if let Some(path) = legacy_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(store) {
            if std::fs::write(&path, json).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
    }
}

/// Persist labels + write secrets to the keyring.
pub fn save_keys(keys: &[KeyEntry]) {
    let store = KeyStore {
        keys: keys
            .iter()
            .map(|k| KeyEntry {
                label: k.label.clone(),
                key: String::new(), // never persist secrets to disk
            })
            .collect(),
    };
    save_store(&store);
    for (i, k) in keys.iter().enumerate() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("key-{i}")) {
            let _ = entry.set_password(&k.key);
        }
    }
}