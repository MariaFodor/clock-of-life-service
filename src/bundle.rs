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
}

pub struct Bundle {
    pub manifest: Manifest,
    pub coefficients: Coefficients,
    pub baselines: HashMap<String, Baseline>,
    /// feature key -> {role, grade, citation}; empty if the bundle ships no evidence.json.
    pub evidence: HashMap<String, Evidence>,
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
        // Same fail-closed rule for the literature levers (PR#1 F2/F3): every declared kind is
        // fully checked, and an unknown kind refuses outright — a typo must never slip past the
        // gate and panic mid-request instead.
        for (key, lit) in &coefficients.literature {
            match lit.kind.as_str() {
                "continuous_z" => {
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
        let mut baselines = HashMap::new();
        for iso in &manifest.countries {
            let b: Baseline = read_json(&dir.join("baselines").join(format!("{iso}.json")))?;
            baselines.insert(iso.clone(), b);
        }
        Ok(Bundle { manifest, coefficients, baselines, evidence })
    }
}
