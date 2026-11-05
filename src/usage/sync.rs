//! Price sync: list prices for the models the release's catalogue doesn't know, read
//! from models.dev for the providers that have an account here, kept in
//! `pricing-sync.json` beside the usage database. The dashboard shows what a sync would
//! change before anything is written. The release's own rates always win: a synced rate
//! only prices a model that has none.
use super::pricing::{Catalogue, Rate, SYNC_SOURCE};
use crate::accounts::{Account, Credential, Provider};
use crate::state::App;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The models.dev answer is a few megabytes; anything near this is not it.
const MAX_BYTES: usize = 64 << 20;

/// Absolute, since the atomic write syncs the file's directory, and a relative
/// database path (the default, beside `config.yaml`) has none to name.
pub fn supplement_beside(database: &Path) -> PathBuf {
    std::path::absolute(database).unwrap_or_else(|_| database.to_path_buf()).with_file_name("pricing-sync.json")
}

pub fn supplement_path(app: &App) -> PathBuf {
    supplement_beside(&super::database_path(&app.cfg(), &app.cfg_path))
}

/// One provider worth pricing: the name usage records give it, and the models.dev
/// provider that lists its prices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub provider: String,
    pub source: String,
    pub label: String,
}

/// The providers with an account here that models.dev has prices for.
pub fn targets(accounts: &[Arc<Account>], api: &Value) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for a in accounts {
        let target = match a.provider {
            Provider::Claude => Some(("anthropic", "anthropic".to_string(), "Anthropic".to_string())),
            Provider::Codex => Some(("openai", "openai".into(), "OpenAI".into())),
            Provider::Gemini => Some(("gemini", "google".into(), "Google".into())),
            Provider::Vertex => Some(("vertex", "google-vertex".into(), "Vertex".into())),
            Provider::Xai => Some(("xai", "xai".into(), "xAI".into())),
            Provider::Kimi => Some(("kimi", "moonshotai".into(), "Moonshot".into())),
            Provider::Meta => Some(("meta", "meta".into(), "Meta".into())),
            Provider::Compat => {
                let name = a.group.clone().unwrap_or_else(|| a.label.clone());
                let base_url = match &*a.cred.read() {
                    Credential::ApiKey { base_url, .. } => base_url.clone(),
                    Credential::OAuth(o) => o.base_url.clone(),
                };
                compat_source(&name, base_url.as_deref(), api).map(|source| ("openai-compat", source, name))
            }
            Provider::Antigravity | Provider::Devin | Provider::Kiro => None,
        };
        let Some((provider, source, label)) = target else { continue };
        if !api[&source]["models"].is_object() {
            continue;
        }
        let target = Target { provider: provider.into(), source, label };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

fn trim_url(u: &str) -> String {
    u.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn host(u: &str) -> Option<String> {
    url::Url::parse(u.trim()).ok()?.host_str().map(str::to_ascii_lowercase)
}

/// The models.dev provider an OpenAI-compatible entry stands for: the one with its
/// exact base URL, else one on the same host (the entry's name breaks a tie), else the
/// one named like it.
pub fn compat_source(name: &str, base_url: Option<&str>, api: &Value) -> Option<String> {
    let name = name.trim().to_ascii_lowercase();
    let providers = api.as_object()?;
    if let Some(base) = base_url.map(trim_url).filter(|b| !b.is_empty()) {
        if let Some((id, _)) = providers.iter().find(|(_, p)| p["api"].as_str().is_some_and(|u| trim_url(u) == base)) {
            return Some(id.clone());
        }
        if let Some(h) = host(&base) {
            let mut same: Vec<&String> = providers
                .iter()
                .filter(|(_, p)| p["api"].as_str().and_then(host).as_deref() == Some(&h))
                .map(|(id, _)| id)
                .collect();
            if let Some(id) = same.iter().find(|id| ***id == name) {
                return Some((*id).clone());
            }
            same.sort_by_key(|id| (id.len(), (*id).clone()));
            if let Some(id) = same.first() {
                return Some((*id).clone());
            }
        }
    }
    providers.contains_key(&name).then_some(name)
}

fn nanos(v: &Value) -> Option<i64> {
    let n = v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)?;
    // models.dev prices are USD per million tokens; a rate is nanodollars per token.
    Some((n * 1000.0).round() as i64)
}

/// A models.dev `cost` as a catalogue rate, when it has the two prices a rate needs.
/// A single context tier becomes the rate's long-context prices; a model with several
/// tiers is priced at its base rate for every length.
pub fn rate_from(provider: &str, model: &str, cost: &Value) -> Option<Rate> {
    let input = nanos(&cost["input"])?;
    let output = nanos(&cost["output"])?;
    let tiers = cost["tiers"].as_array().map(Vec::as_slice).unwrap_or_default();
    let tier = match tiers {
        [only] if only["tier"]["type"] == "context" => Some(only),
        _ => None,
    };
    let threshold = tier.and_then(|t| t["tier"]["size"].as_u64()).filter(|n| *n > 0);
    let above = |key: &str| threshold.and_then(|_| nanos(&tier.unwrap()[key]));
    let (threshold, above_input, above_output) = match (threshold, above("input"), above("output")) {
        (Some(t), Some(i), Some(o)) => (Some(t), Some(i), Some(o)),
        _ => (None, None, None),
    };
    Some(Rate {
        provider: provider.into(),
        models: vec![model.into()],
        effective_from: None,
        effective_until: None,
        source_url: SYNC_SOURCE.into(),
        input_nanos_per_token: input,
        cache_read_nanos_per_token: nanos(&cost["cache_read"]),
        cache_write_nanos_per_token: nanos(&cost["cache_write"]),
        cache_write_5m_nanos_per_token: None,
        cache_write_1h_nanos_per_token: None,
        cache_write_30m_nanos_per_token: None,
        cache_write_12h_nanos_per_token: None,
        output_nanos_per_token: output,
        tier_multipliers: Default::default(),
        threshold_input_tokens: threshold,
        above_threshold_input_nanos_per_token: above_input,
        above_threshold_read_nanos_per_token: threshold.and_then(|_| above("cache_read")),
        above_threshold_write_nanos_per_token: threshold.and_then(|_| above("cache_write")),
        above_threshold_write_5m_nanos_per_token: None,
        above_threshold_write_1h_nanos_per_token: None,
        above_threshold_write_30m_nanos_per_token: None,
        above_threshold_write_12h_nanos_per_token: None,
        above_threshold_output_nanos_per_token: above_output,
        long_context_excluded_tiers: Vec::new(),
    })
}

/// A rate as the page shows it: USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub provider: String,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub free: bool,
    pub long_context: bool,
}

impl Entry {
    fn of(r: &Rate) -> Self {
        let per_m = |n: i64| n as f64 / 1000.0;
        Self {
            provider: r.provider.clone(),
            model: r.models.first().cloned().unwrap_or_default(),
            input: per_m(r.input_nanos_per_token),
            output: per_m(r.output_nanos_per_token),
            cache_read: r.cache_read_nanos_per_token.map(per_m),
            cache_write: r.cache_write_nanos_per_token.map(per_m),
            free: r.input_nanos_per_token == 0 && r.output_nanos_per_token == 0,
            long_context: r.threshold_input_tokens.is_some(),
        }
    }
}

fn same_price(a: &Rate, b: &Rate) -> bool {
    (
        a.input_nanos_per_token,
        a.output_nanos_per_token,
        a.cache_read_nanos_per_token,
        a.cache_write_nanos_per_token,
        a.threshold_input_tokens,
        a.above_threshold_input_nanos_per_token,
        a.above_threshold_output_nanos_per_token,
        a.above_threshold_read_nanos_per_token,
        a.above_threshold_write_nanos_per_token,
    ) == (
        b.input_nanos_per_token,
        b.output_nanos_per_token,
        b.cache_read_nanos_per_token,
        b.cache_write_nanos_per_token,
        b.threshold_input_tokens,
        b.above_threshold_input_nanos_per_token,
        b.above_threshold_output_nanos_per_token,
        b.above_threshold_read_nanos_per_token,
        b.above_threshold_write_nanos_per_token,
    )
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub entry: Entry,
    pub before: Entry,
}

#[derive(Debug, Serialize)]
pub struct Provider_ {
    #[serde(flatten)]
    pub target: Target,
    pub models: usize,
}

/// What a sync would write: the new file's rates, and how they differ from the current one.
#[derive(Debug, Serialize)]
pub struct Plan {
    pub source: &'static str,
    pub providers: Vec<Provider_>,
    pub added: Vec<Entry>,
    pub changed: Vec<Change>,
    /// Models priced by the release's catalogue, which a sync leaves alone.
    pub bundled: usize,
    pub unchanged: usize,
    /// Synced earlier for a provider or model models.dev no longer lists; kept as they are.
    pub kept: usize,
    #[serde(skip)]
    pub rates: Vec<Rate>,
}

impl Plan {
    pub fn changes(&self) -> usize {
        self.added.len() + self.changed.len()
    }
}

/// Compares what models.dev lists for the targets against the catalogue (without its
/// supplement) and the supplement on disk.
pub fn plan(base: &Catalogue, current: &[Rate], targets: &[Target], api: &Value) -> Plan {
    let mut plan = Plan {
        source: SYNC_SOURCE,
        providers: Vec::new(),
        added: Vec::new(),
        changed: Vec::new(),
        bundled: 0,
        unchanged: 0,
        kept: 0,
        rates: Vec::new(),
    };
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    for t in targets {
        let Some(models) = api[&t.source]["models"].as_object() else { continue };
        let mut count = 0;
        for (model, m) in models {
            let Some(rate) = rate_from(&t.provider, model, &m["cost"]) else { continue };
            if !seen.insert((t.provider.clone(), model.clone())) {
                continue;
            }
            count += 1;
            if base.has_rate(&t.provider, model) {
                plan.bundled += 1;
                continue;
            }
            match current.iter().find(|r| r.provider == t.provider && r.models.contains(model)) {
                Some(old) if same_price(old, &rate) => plan.unchanged += 1,
                Some(old) => plan.changed.push(Change { entry: Entry::of(&rate), before: Entry::of(old) }),
                None => plan.added.push(Entry::of(&rate)),
            }
            plan.rates.push(rate);
        }
        plan.providers.push(Provider_ { target: t.clone(), models: count });
    }
    for old in current {
        let key = (old.provider.clone(), old.models.first().cloned().unwrap_or_default());
        if !seen.contains(&key) && !old.models.iter().any(|m| base.has_rate(&old.provider, m)) {
            plan.kept += 1;
            plan.rates.push(old.clone());
        }
    }
    plan.added.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
    plan.changed.sort_by(|a, b| (&a.entry.provider, &a.entry.model).cmp(&(&b.entry.provider, &b.entry.model)));
    plan
}

pub async fn fetch(http: &reqwest::Client) -> Result<Value> {
    let resp =
        http.get(SYNC_SOURCE).timeout(std::time::Duration::from_secs(45)).send().await.context("reach models.dev")?;
    if !resp.status().is_success() {
        bail!("models.dev answered HTTP {}", resp.status().as_u16());
    }
    let bytes = resp.bytes().await.context("read the models.dev answer")?;
    if bytes.len() > MAX_BYTES {
        bail!("the models.dev answer is too large");
    }
    let v: Value = serde_json::from_slice(&bytes).context("parse the models.dev answer")?;
    if !v.is_object() {
        bail!("the models.dev answer is not a provider list");
    }
    Ok(v)
}

/// The supplement on disk, as the status endpoint reports it.
pub fn status(path: &Path) -> Value {
    if !path.exists() {
        return json!({ "path": path, "synced": false, "rate_count": 0 });
    }
    match Catalogue::read_supplement(path) {
        Ok(c) => {
            json!({ "path": path, "synced": true, "version": c.version, "verified_at": c.verified_at, "rate_count": c.rates.len() })
        }
        Err(e) => json!({ "path": path, "synced": false, "rate_count": 0, "error": e.to_string() }),
    }
}

fn overrides(app: &App) -> Option<PathBuf> {
    app.cfg().usage.pricing_overrides.as_deref().map(PathBuf::from)
}

async fn prepare(app: &App) -> Result<(Plan, PathBuf)> {
    let api = fetch(&app.http.control(None)).await?;
    let overrides = overrides(app);
    let base = Catalogue::load(overrides.as_deref())?;
    let path = supplement_path(app);
    let current =
        if path.exists() { Catalogue::read_supplement(&path).map(|c| c.rates).unwrap_or_default() } else { Vec::new() };
    let plan = plan(&base, &current, &targets(&app.pool.all(), &api), &api);
    Ok((plan, path))
}

fn summary(plan: &Plan, path: &Path) -> Value {
    let mut v = serde_json::to_value(plan).unwrap_or_default();
    v["changes"] = plan.changes().into();
    v["rate_count"] = plan.rates.len().into();
    v["path"] = json!(path);
    v
}

/// What a sync would change, without writing anything.
pub async fn preview(app: &App) -> Result<Value> {
    let (plan, path) = prepare(app).await?;
    Ok(summary(&plan, &path))
}

/// Writes the synced prices, reloads the catalogue and prices earlier unpriced usage.
pub async fn apply(app: &App) -> Result<Value> {
    let Some(store) = app.usage.clone() else { bail!("usage analytics is off") };
    let (plan, path) = prepare(app).await?;
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let file = Catalogue {
        version: format!("models.dev-{today}"),
        verified_at: today,
        rates: plan.rates.clone(),
        local_override: false,
        currency: "USD".into(),
    };
    crate::files::write_private(&path, &serde_json::to_vec_pretty(&file)?)?;
    let catalogue = Catalogue::load_with_supplement(overrides(app).as_deref(), Some(&path))?;
    let version = catalogue.version.clone();
    let repriced = store.replace_catalogue(catalogue).await?;
    let mut v = summary(&plan, &path);
    v["applied"] = true.into();
    v["repriced"] = repriced.into();
    v["catalogue_version"] = version.into();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> Value {
        json!({
            "opencode": { "name": "OpenCode", "api": "https://opencode.ai/inference/openai/v1", "models": {
                "glm-5.3": { "cost": { "input": 1.4, "output": 4.4, "cache_read": 0.26 } },
                "big-pickle": { "cost": { "input": 0, "output": 0, "cache_read": 0 } },
                "no-price": { "name": "x" }
            } },
            "opencode-go": { "name": "OpenCode Go", "api": "https://opencode.ai/go/v1", "models": {} },
            "openai": { "name": "OpenAI", "models": {
                "gpt-6-astra": { "cost": { "input": 10, "output": 50, "cache_read": 1, "cache_write": 12.5,
                    "tiers": [{ "input": 20, "output": 75, "cache_read": 2, "cache_write": 25, "tier": { "type": "context", "size": 272000 } }] } },
                "gpt-new": { "cost": { "input": 0.5, "output": 2.5, "cache_read": 0.05 } }
            } },
            "qwen": { "name": "Qwen", "api": "https://dashscope.example/v1", "models": {
                "q": { "cost": { "input": 0.1, "output": 0.4, "tiers": [
                    { "input": 0.2, "output": 0.8, "tier": { "type": "context", "size": 32000 } },
                    { "input": 0.3, "output": 1.2, "tier": { "type": "context", "size": 256000 } }] } }
            } }
        })
    }

    #[test]
    fn a_compat_entry_finds_its_provider_by_url_then_host_then_name() {
        let api = api();
        assert_eq!(
            compat_source("x", Some("https://opencode.ai/inference/openai/v1/"), &api).as_deref(),
            Some("opencode")
        );
        assert_eq!(
            compat_source("opencode", Some("https://opencode.ai/inference/v1"), &api).as_deref(),
            Some("opencode")
        );
        assert_eq!(
            compat_source("whatever", Some("https://opencode.ai/zen"), &api).as_deref(),
            Some("opencode"),
            "shortest id on the host"
        );
        assert_eq!(compat_source("qwen", Some("http://localhost:11434/v1"), &api).as_deref(), Some("qwen"), "by name");
        assert_eq!(compat_source("ollama", Some("http://localhost:11434/v1"), &api), None);
        assert_eq!(compat_source("Qwen", None, &api).as_deref(), Some("qwen"));
    }

    #[test]
    fn prices_convert_to_nanodollars_with_one_context_tier() {
        let api = api();
        let r = rate_from("openai", "gpt-6-astra", &api["openai"]["models"]["gpt-6-astra"]["cost"]).unwrap();
        assert_eq!((r.input_nanos_per_token, r.output_nanos_per_token), (10_000, 50_000));
        assert_eq!((r.cache_read_nanos_per_token, r.cache_write_nanos_per_token), (Some(1_000), Some(12_500)));
        assert_eq!(r.threshold_input_tokens, Some(272_000));
        assert_eq!(r.above_threshold_input_nanos_per_token, Some(20_000));
        assert_eq!(r.above_threshold_output_nanos_per_token, Some(75_000));
        assert_eq!(r.above_threshold_write_nanos_per_token, Some(25_000));
        let q = rate_from("openai-compat", "q", &api["qwen"]["models"]["q"]["cost"]).unwrap();
        assert_eq!(q.threshold_input_tokens, None, "several tiers price at the base rate");
        assert_eq!(q.input_nanos_per_token, 100);
        assert!(rate_from("x", "y", &api["opencode"]["models"]["no-price"]["cost"]).is_none());
        let free = rate_from("openai-compat", "big-pickle", &api["opencode"]["models"]["big-pickle"]["cost"]).unwrap();
        assert!(Entry::of(&free).free);
        assert!(!Entry::of(&q).free);
        assert_eq!(Entry::of(&r).input, 10.0);
    }

    #[test]
    fn a_plan_adds_what_the_catalogue_lacks_and_leaves_its_own_rates_alone() {
        let api = api();
        let base = Catalogue::load(None).unwrap();
        let targets = vec![
            Target { provider: "openai".into(), source: "openai".into(), label: "OpenAI".into() },
            Target { provider: "openai-compat".into(), source: "opencode".into(), label: "opencode".into() },
        ];
        let p = plan(&base, &[], &targets, &api);
        assert_eq!(p.bundled, 1, "gpt-6-astra is priced by the release");
        let added: Vec<_> = p.added.iter().map(|e| (e.provider.as_str(), e.model.as_str())).collect();
        assert_eq!(added, vec![("openai", "gpt-new"), ("openai-compat", "big-pickle"), ("openai-compat", "glm-5.3")]);
        assert_eq!(p.rates.len(), 3);
        assert_eq!(p.providers.iter().map(|p| p.models).collect::<Vec<_>>(), vec![2, 2]);
        // A second sync against the written file: nothing new unless a price moved.
        let mut current = p.rates.clone();
        let glm = current.iter().position(|r| r.models == ["glm-5.3"]).unwrap();
        current[glm].input_nanos_per_token = 1;
        let mut gone = current[0].clone();
        gone.models = vec!["gone-model".into()];
        current.push(gone);
        let again = plan(&base, &current, &targets, &api);
        assert_eq!((again.added.len(), again.unchanged, again.kept), (0, 2, 1));
        assert_eq!(again.changed.len(), 1);
        assert_eq!(again.changed[0].before.input, 0.001);
        assert_eq!(again.changed[0].entry.input, 1.4);
        assert_eq!(again.rates.len(), 4, "the kept rate stays in the file");
        // The written supplement loads on top of the bundled catalogue.
        let dir = std::env::temp_dir().join(format!("fusebox-sync-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pricing-sync.json");
        let file = Catalogue {
            version: "models.dev-test".into(),
            verified_at: "2026-10-29".into(),
            rates: again.rates.clone(),
            local_override: false,
            currency: "USD".into(),
        };
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let merged = Catalogue::load_with_supplement(None, Some(&path)).unwrap();
        assert_eq!(merged.rates.len(), base.rates.len() + 4);
        assert!(merged.version.ends_with("+models.dev-test"));
        assert!(!merged.local_override);
        assert_eq!(merged.synced_rates().count(), 4);
        assert_eq!(status(&path)["rate_count"], 4);
        assert_eq!(status(&dir.join("none.json"))["synced"], false);
        // A supplement that claims another provenance is refused, and a missing file is fine.
        let mut bad = file.clone();
        bad.rates[0].source_url = "https://example.com".into();
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(Catalogue::read_supplement(&path).is_err());
        assert_eq!(
            Catalogue::load_with_supplement(None, Some(&path)).unwrap().rates.len(),
            base.rates.len(),
            "a bad file is skipped"
        );
        assert_eq!(
            Catalogue::load_with_supplement(None, Some(&dir.join("none.json"))).unwrap().rates.len(),
            base.rates.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn synced_rates_price_usage_the_release_cannot() {
        use super::super::types::Observation;
        let api = api();
        let base = Catalogue::load(None).unwrap();
        let targets =
            vec![Target { provider: "openai-compat".into(), source: "opencode".into(), label: "opencode".into() }];
        let p = plan(&base, &[], &targets, &api);
        let mut c = base.clone();
        c.rates.extend(p.rates);
        let mut o = Observation::new("proxy", "id".into(), "openai-compat", Utc::now().timestamp_millis());
        o.actual_model = Some("glm-5.3".into());
        o.tokens.input = Some(1_000_000);
        o.tokens.output = Some(1_000_000);
        o.tokens.cache_read = Some(0);
        o.tokens.cache_write = Some(0);
        o.completeness = "complete".into();
        let s = c.price(&o);
        assert_eq!(s.cost_nanos, Some(5_800_000_000), "$1.40 in + $4.40 out: {s:?}");
        assert_eq!(s.basis, "current_rate_equivalent");
        assert_eq!(base.price(&o).basis, "unknown_model");
    }
}
