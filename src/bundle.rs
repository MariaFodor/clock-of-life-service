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

#[derive(Deserialize)]
pub struct Coefficients {
    pub prediction: HashMap<String, f64>,
    pub attribution: HashMap<String, f64>,
    pub standardizer: HashMap<String, Stat>,
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
