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

fn parse_usage(v: &serde_json::Value) -> (Vec<(String, f64)>, Vec<(String, u64)>, Option<String>) {
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
    (windows, models, models_window)
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
                let (windows, models, models_window) = parse_usage(&v);
                if windows.is_empty() {
                    Usage {
                        error: Some("no usage windows in response".into()),
                        windows,
                        models,
                        models_window,
                    }
                } else {
                    Usage { error: None, windows, models, models_window }
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

#[derive(Serialize, Deserialize, Default)]
pub struct KeyStore {
    pub keys: Vec<KeyEntry>,
}

impl KeyStore {
    pub fn load() -> Self {
        if let Some(path) = store_path() {
            if let Ok(txt) = std::fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str(&txt) {
                    return store;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        if let Some(path) = store_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

fn store_path() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .map(|h| h.join(".ollama-shepherd").join("keys.json"))
}