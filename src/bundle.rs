//! Load and verify a model artifact bundle produced by `clock-of-life-model`.
//!
//! The bundle is the model version and the reproducibility unit. It is verified against the checksums in
//! its own manifest at load time — no trust-by-default path (ADR-006 / principle 12).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize, Clone)]
pub struct Stat {
    pub mean: f64,
    pub sd: f64,
}

/// A literature lever's centring reference: `kind: "mean"` (z = 0 at the standardizer mean) or
/// `kind: "level"` with the reference category in `level` (e.g. alcohol centred on "light").
#[derive(Deserialize, Clone)]
pub struct LitReference {
    pub kind: String,
    #[serde(default)]
    pub level: Option<String>,
}

/// One literature-sourced feature (diet/alcohol/sedentary/stress/env): appended to the fitted linear
/// predictor at its stated confidence, never fitted on the cohort (see the model's literature.py).
#[derive(Deserialize, Clone)]
pub struct LiteratureFeature {
    pub kind: String, // "continuous_z" | "categorical_monotonic" | "precomputed"
    #[serde(default)]
    pub beta: Option<f64>,
    #[serde(default)]
    pub levels: Option<HashMap<String, f64>>,
    #[serde(default)]
    pub reference: Option<LitReference>,
    pub grade: String,
    #[serde(default)]
    pub citation: String,
}

#[derive(Deserialize)]
pub struct Coefficients {
    pub prediction: HashMap<String, f64>,
    pub attribution: HashMap<String, f64>,
    pub standardizer: HashMap<String, Stat>,
    /// Per-lever TOTAL effects: each fitted on the adjustment set the causal graph implies
    /// (confounders only, never mediators) and precision-weighted against its literature prior.
    /// They answer "what changes if you change this", which is what What-If and the
    /// recommendations need. Never summed with `prediction` — that would double-count every
    /// mediated path. Empty for pre-v3.0.0 bundles.
    #[serde(default)]
    pub total_effect: HashMap<String, f64>,
    #[serde(default)]
    pub total_effect_sd: HashMap<String, f64>,
    /// Neutral values for optional inputs, supplied by the model rather than invented here.
    /// Currently `cigs_day_when_current_smoker`: since the smoking contrast was corrected,
    /// `smk_current` no longer absorbs dose, so a smoker who skips the dose question must be
    /// scored at the smokers' mean and not at zero.
    #[serde(default)]
    pub conditional_defaults: HashMap<String, f64>,
    /// Pre-v2.2.0 bundles ship this without standardizers/references and are refused by the gate.
    #[serde(default)]
    pub literature: HashMap<String, LiteratureFeature>,
    pub young_cutoff: f64,
}

#[derive(Deserialize)]
pub struct RefLp {
    pub young: f64,
    pub old: f64,
}

#[derive(Deserialize)]
pub struct Baseline {
    pub country: String,
    /// sex ("M"/"F") -> age (as string) -> probability of death that year
    pub qx: HashMap<String, HashMap<String, f64>>,
    pub reference_lp: RefLp,
    pub national_le_40: HashMap<String, f64>,
}

#[derive(Deserialize)]
pub struct Manifest {
    pub version: String,
    pub algorithm: String,
    pub countries: Vec<String>,
    pub checksums: HashMap<String, String>,
    /// Provenance used to seed the `model_version` row (optional — absent in older bundles).
    #[serde(default)]
    pub reference_population: Option<String>,
    #[serde(default)]
    pub data_as_of: Option<String>,
}

/// Per-factor evidence shipped with the model (`evidence.json`): role, grade, and citation.
#[derive(Deserialize, Clone, Default)]
pub struct Evidence {
    pub role: String,
    pub grade: String,
    pub citation: String,
    /// DOI of the backing paper, verified against Crossref when the ontology entry was written —
    /// a citation nobody can open is not a citation.
    #[serde(default)]
    pub doi: Option<String>,
    /// Resolvable link, so the web can put the actual article behind each indicator.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub first_author: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub study_slug: Option<String>,
}

pub struct Bundle {
    pub manifest: Manifest,
    pub coefficients: Coefficients,
    pub baselines: HashMap<String, Baseline>,
    /// feature key -> {role, grade, citation, doi, url}; empty if the bundle ships no evidence.json.
    pub evidence: HashMap<String, Evidence>,
    /// The full ontology as shipped (roles, sign/shape constraints, causal graph, verified
    /// citations). `Null` for pre-v3.0.0 bundles.
    pub ontology: serde_json::Value,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// First 16 hex chars of the file's SHA-256 (matches the model exporter's checksum format).
fn checksum16(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex(&digest)[..16].to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Bundle {
    pub fn load(dir: &Path) -> Result<Bundle, String> {
        let manifest: Manifest = read_json(&dir.join("manifest.json"))?;

        // verify every file the manifest claims (integrity gate)
        for (rel, expected) in &manifest.checksums {
            let got = checksum16(&dir.join(rel))?;
            if &got != expected {
                return Err(format!("checksum mismatch for {rel}: {got} != {expected}"));
            }
        }

        let coefficients: Coefficients = read_json(&dir.join("coefficients.json"))?;
        // The checksum gate proves integrity, not usability: a bundle whose standardizer lacks a key
        // the scoring design z-scores would panic inside the estimate handler. Refuse it here, at
        // startup, like any other bad bundle (REVIEW-2026-09-09 S4).
        for key in crate::scoring::STANDARDIZED_KEYS {
            if !coefficients.standardizer.contains_key(*key) {
                return Err(format!(
                    "coefficients.json: standardizer is missing '{key}', which the scoring design \
                     requires — refusing this bundle"
                ));
            }
        }
        // A coefficient the design never emits is scored as if it were zero — v2.2.0's bmi
        // (-1.006 per SD) would vanish silently on rollback. Refuse instead: "the bundle loads"
        // has to mean "the bundle is scored as its author intended".
        {
            let emitted: std::collections::HashSet<&str> =
                crate::scoring::DESIGN_KEYS.iter().copied().collect();
            let orphans: Vec<&String> = coefficients
                .prediction
                .keys()
                .filter(|k| !emitted.contains(k.as_str()))
                .collect();
            if !orphans.is_empty() {
                return Err(format!(
                    "coefficients.json: prediction has coefficient(s) {orphans:?} that the scoring \
                     design never emits — they would be silently ignored. Refusing this bundle."
                ));
            }
        }

        // Same fail-closed rule for the literature levers (PR#1 F2/F3): every declared kind is
        // fully checked, and an unknown kind refuses outright — a typo must never slip past the
        // gate and panic mid-request instead.
        for (key, lit) in &coefficients.literature {
            match lit.kind.as_str() {
                "continuous_z" => {
                    // A continuous lever without a beta would load fine and silently contribute
                    // 0.0 for every answer — the lever disappears with no error (PR#1 re-review).
                    if lit.beta.is_none() {
                        return Err(format!(
                            "coefficients.json: literature lever '{key}' is continuous_z but ships \
                             no beta — refusing this bundle"
                        ));
                    }
                    if !coefficients.standardizer.contains_key(key) {
                        return Err(format!(
                            "coefficients.json: literature lever '{key}' is continuous_z but has no \
                             standardizer entry — refusing this bundle"
                        ));
                    }
                    // The scorer assumes mean-centring (z(reference) = 0); any other centring would
                    // be silently ignored, so it is refused instead.
                    if lit.reference.as_ref().map(|r| r.kind.as_str()) != Some("mean") {
                        return Err(format!(
                            "coefficients.json: continuous literature lever '{key}' must declare \
                             reference kind \"mean\" — refusing this bundle"
                        ));
                    }
                }
                "categorical_monotonic" => {
                    let levels = lit.levels.as_ref().ok_or_else(|| format!(
                        "coefficients.json: literature lever '{key}' is categorical but ships no \
                         levels — refusing this bundle"
                    ))?;
                    if key == "alcohol" {
                        for lvl in crate::scoring::ALCOHOL_LEVELS {
                            if !levels.contains_key(*lvl) {
                                return Err(format!(
                                    "coefficients.json: alcohol levels are missing '{lvl}' — a \
                                     missing level would silently score 0.0 — refusing this bundle"
                                ));
                            }
                        }
                    }
                    let reference = lit.reference.as_ref().and_then(|r| r.level.as_deref());
                    match reference {
                        Some(r) if levels.contains_key(r) => {}
                        _ => {
                            return Err(format!(
                                "coefficients.json: categorical literature lever '{key}' must \
                                 declare a centring reference level present in its levels — \
                                 refusing this bundle"
                            ));
                        }
                    }
                }
                // "precomputed": the term is computed by the service (env_term hardcodes the
                // RES-04 formula); the shipped beta is intentionally not consumed.
                "precomputed" => {}
                other => {
                    return Err(format!(
                        "coefficients.json: literature lever '{key}' has unknown kind '{other}' — \
                         refusing this bundle"
                    ));
                }
            }
        }
        // Evidence is supplementary (powers the "Why?" citations); tolerate its absence.
        let evidence: HashMap<String, Evidence> =
            read_json(&dir.join("evidence.json")).unwrap_or_default();
        // The ontology travels with the model (v3.0.0+): roles, causal edges and verified article
        // links come from the same file the fit was constrained by, rather than a second copy that
        // can drift from it. Served verbatim so the web can draw the graph the model actually used.
        let ontology: serde_json::Value =
            read_json(&dir.join("ontology.json")).unwrap_or(serde_json::Value::Null);
        let mut baselines = HashMap::new();
        for iso in &manifest.countries {
            let b: Baseline = read_json(&dir.join("baselines").join(format!("{iso}.json")))?;
            baselines.insert(iso.clone(), b);
        }
        Ok(Bundle { manifest, coefficients, baselines, evidence, ontology })
    }
}
