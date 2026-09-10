//! Load and verify a model artifact bundle produced by `clock-of-life-model`.
//!
//! The bundle is the model version and the reproducibility unit. It is verified against the checksums in
//! its own manifest at load time — no trust-by-default path (ADR-006 / principle 12).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
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

/// What an average person in a country is actually exposed to — the value the ENV term is CENTRED on.
///
/// This replaces two hardcoded constants (`RO_PM25_REF = 14.0`, `RO_NDVI_REF = 0.5`) that were wrong
/// for Romania and applied to every country. WHO's measured Romanian total for 2023 is 10.412 and
/// Bucharest's population-weighted NDVI is 0.2539.
///
/// `ndvi` is `Option` and stays one all the way through: a country nobody has measured has no
/// greenness reference, and a `0.0` there would be priced as "this country is barren". `ndvi_cities`
/// is how many cities the national figure was derived from — 22 of the 30 scoreable countries rest on
/// exactly one, and anything that shows the number has to be able to say so.
#[derive(Deserialize, Serialize, Clone)]
pub struct EnvReference {
    pub pm25: f64,
    pub pm25_low: Option<f64>,
    pub pm25_high: Option<f64>,
    pub pm25_year: Option<i32>,
    #[serde(default)]
    pub pm25_by_area: HashMap<String, f64>,

    pub ndvi: Option<f64>,
    pub ndvi_year: Option<i32>,
    #[serde(default)]
    pub ndvi_cities: i32,
    pub ndvi_weighted: Option<bool>,
    /// The cities the national figure was derived from — `None`, not `[]`, where there are none. An
    /// empty vec would be indistinguishable from "derived from cities, but we lost the list".
    ///
    /// `Option` rather than `#[serde(default)]`, because `serde(default)` fills in only for an ABSENT
    /// field and the model emits an explicit `null`. That difference failed ten tests on Andorra, which
    /// has a measured air figure and no green city at all.
    pub ndvi_derived_from: Option<Vec<String>>,
}

/// One real settlement with a measured PM2.5 reading, and greenness that is either its own or its
/// country's — `ndvi_basis` says which, and that word is not optional.
///
/// These replace the seven invented Romanian rows that `seeds/locations.json` described as
/// "ILLUSTRATIVE" and that shipped anyway.
#[derive(Deserialize, Serialize, Clone)]
pub struct Place {
    pub iso3: String,
    pub city: String,
    pub lat: f64,
    pub lon: f64,
    pub population: Option<i64>,
    pub pm25: f64,
    pub pm25_year: i32,
    pub pm25_stations: Option<i32>,
    pub pm25_temporal_coverage: Option<f64>,
    pub ndvi: Option<f64>,
    pub ndvi_year: Option<i32>,
    /// "city" — measured in this settlement. "country" — this country's figure, shown here because
    /// this settlement has none. `None` — no greenness at all. The screen must print the distinction.
    pub ndvi_basis: Option<String>,
    pub ndvi_matched_city: Option<String>,
    pub ndvi_distance_km: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Licence {
    pub licence: String,
    pub url: Option<String>,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub share_alike: bool,
    #[serde(default)]
    pub non_commercial: bool,
}

#[derive(Deserialize)]
pub struct Baseline {
    pub country: String,
    /// sex ("M"/"F") -> age (as string) -> probability of death that year
    pub qx: HashMap<String, HashMap<String, f64>>,
    pub reference_lp: RefLp,
    pub national_le_40: HashMap<String, f64>,
    /// Absent for a country WHO has never measured. The scoring path must then refuse to price the
    /// environment rather than price it against somebody else's country.
    pub env_reference: Option<EnvReference>,
}

/// A country the atlas may DRAW but the clock may not score.
///
/// Deliberately a different type from `Baseline`, and deliberately without a `reference_lp` field —
/// not an `Option`. The Cox reference person is centred on national smoking and overweight rates,
/// which exist for Europe; a country scored without them gets a reference person built from US cohort
/// means, and its reader gets a confident, wrong, PERSONAL number. Making that impossible to express
/// beats making it a runtime check someone can forget: the scoring path cannot reach one of these,
/// because the type does not carry what `risk()` needs.
#[derive(Deserialize)]
pub struct ReferenceBaseline {
    pub country: String,
    pub iso3: Option<String>,
    pub name: Option<String>,
    pub region: Option<String>,
    pub lifetable_year: Option<i32>,
    /// sex ("M"/"F"/"B") -> age (as string) -> probability of death that year
    pub qx: HashMap<String, HashMap<String, f64>>,
    /// Carried on the reference set too, so the atlas can draw exposure for the 85 countries that have
    /// it without the map having to reach into the scoreable 30.
    pub env_reference: Option<EnvReference>,
}

#[derive(Deserialize)]
pub struct Manifest {
    pub version: String,
    pub algorithm: String,
    /// The SCOREABLE set. `/api/meta` is derived from this, and an entry here promises a person from
    /// that country can be given a number centred on their own population.
    pub countries: Vec<String>,
    /// Everything with a life table — a superset, for the atlas to draw. Absent in bundles before v4.
    #[serde(default)]
    pub reference_countries: Vec<String>,
    /// Eurostat calls Greece EL; ISO — and this bundle — call it GR. Stored calculations and deployed
    /// clients still say EL, so it is normalised rather than 400ed.
    #[serde(default)]
    pub country_aliases: HashMap<String, String>,
    /// Where the life tables came from — dataset, publisher, licence, citation, per-file digests and
    /// a retrieval date. Served verbatim by the atlas so the page attributes what it draws from the
    /// artifact rather than from a string somebody typed into the front end.
    #[serde(default)]
    pub sources: Vec<serde_json::Value>,
    /// Where the exposure VALUES came from, beside where the exposure RESPONSE came from.
    #[serde(default)]
    pub env_sources: Option<serde_json::Value>,
    /// The licences this bundle's CONTENTS oblige — WHO's air data is CC BY-NC-SA 3.0 IGO, which is
    /// share-alike and attaches to anything distributed containing it, this service included.
    #[serde(default)]
    pub licences: Option<Vec<Licence>>,
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
    /// Every country with a life table, scoreable or not — what the atlas may draw. Empty for
    /// pre-v4.0.0 bundles, which had only the scoreable set.
    pub reference: HashMap<String, ReferenceBaseline>,
    /// Every real settlement with a measured PM2.5 reading. Empty for a pre-v4.1.0 bundle, which is
    /// the case the seeder must refuse rather than quietly leave the invented rows in place.
    pub places: Vec<Place>,
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

/// The same 16-hex digest the manifest uses, over a string rather than a file — used to give the
/// atlas payload a strong validator that changes whenever the payload does, including a rebuild at
/// the same version.
pub fn checksum16_of(body: &str) -> String {
    hex(&Sha256::digest(body.as_bytes())[..8])
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
        // The ontology travels with the model (v3.0.0+): roles, causal edges and verified article
        // links come from the same file the fit was constrained by, rather than a second copy that
        // can drift from it. Served verbatim so the web can draw the graph the model actually used.
        let ontology: serde_json::Value =
            read_json(&dir.join("ontology.json")).unwrap_or(serde_json::Value::Null);
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

        // Every lever the ontology declares must be one this build can actually explain. cigs_day
        // shipped as a lever with a total effect while being absent from the attribution surface,
        // so over a year of smoking harm was dropped from why[] and from the ranking — invisibly,
        // because nothing checked the ontology against the code in this direction.
        if let Some(ont) = ontology.as_object() {
            let surfaced: std::collections::HashSet<&str> =
                crate::scoring::surfaced_keys().collect();
            let unsurfaced: Vec<&String> = ont
                .iter()
                .filter(|(k, v)| {
                    !k.starts_with('_')
                        && v.get("role").and_then(|r| r.as_str()) == Some("lever")
                        && coefficients.total_effect.contains_key(k.as_str())
                        && !surfaced.contains(k.as_str())
                })
                .map(|(k, _)| k)
                .collect();
            if !unsurfaced.is_empty() {
                return Err(format!(
                    "ontology declares lever(s) {unsurfaced:?} with a total effect that this build \
                     never surfaces in why[] — their contribution would be silently dropped from \
                     the breakdown and the ranking. Refusing this bundle."
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

        // Country codes come from a hand-copied manifest and are joined onto a directory path, so
        // they are shape-checked before being used as a filename.
        fn iso_ok(iso: &str) -> bool {
            iso.len() == 2 && iso.bytes().all(|b| b.is_ascii_uppercase())
        }
        // `remaining_le` indexes pts[0]; an empty table panics inside a request handler, which is the
        // wrong place to discover a bad bundle. Applied to EVERY table loaded, not just the reference
        // ones — a manifest with no `reference_countries` would otherwise skip the check entirely and
        // leave the panic reachable through a scoreable country.
        fn qx_ok(iso: &str, qx: &HashMap<String, HashMap<String, f64>>) -> Result<(), String> {
            if qx.is_empty() {
                return Err(format!("baselines/{iso}.json: qx carries no sexes at all"));
            }
            for (sex, table) in qx {
                if table.is_empty() {
                    return Err(format!(
                        "baselines/{iso}.json: qx for sex '{sex}' is empty — refusing this bundle"
                    ));
                }
                // Contiguous, because the two consumers disagree about a hole: `remaining_le`
                // interpolates across one, while `adult_mortality_15_60` reads a missing age as zero
                // mortality that year. An abridged five-year table would make Romania's 45q15 read
                // 31.8 instead of 179.6 — plausible, and wrong.
                let mut ages: Vec<i64> = table.keys().filter_map(|a| a.parse().ok()).collect();
                if ages.len() != table.len() {
                    return Err(format!("baselines/{iso}.json: qx for '{sex}' has a non-numeric age"));
                }
                ages.sort_unstable();
                if ages.windows(2).any(|w| w[1] != w[0] + 1) {
                    return Err(format!(
                        "baselines/{iso}.json: qx for '{sex}' skips an age — refusing this bundle"
                    ));
                }
            }
            Ok(())
        }

        let mut baselines = HashMap::new();
        for iso in &manifest.countries {
            if !iso_ok(iso) {
                return Err(format!("manifest: {iso:?} is not an ISO 3166-1 alpha-2 code"));
            }
            let b: Baseline = read_json(&dir.join("baselines").join(format!("{iso}.json")))?;
            qx_ok(iso, &b.qx)?;
            baselines.insert(iso.clone(), b);
        }

        // The reference set: every country the atlas may draw. Loaded into a type that cannot be
        // scored against, so widening this list can never widen /api/meta.
        let mut reference = HashMap::new();
        for iso in &manifest.reference_countries {
            if !iso_ok(iso) {
                return Err(format!("manifest: {iso:?} is not an ISO 3166-1 alpha-2 code"));
            }
            let b: ReferenceBaseline = read_json(&dir.join("baselines").join(format!("{iso}.json")))?;
            qx_ok(iso, &b.qx)?;
            reference.insert(iso.clone(), b);
        }
        // The settlements. Absent for a pre-v4.1.0 bundle — allowed to be empty here so the service
        // still boots on an older artifact, and refused where it MATTERS: the seeder will not run
        // against an empty list, because "no places" would silently leave the invented rows in place.
        let places_path = dir.join("places.json");
        let places: Vec<Place> = if places_path.exists() { read_json(&places_path)? } else { Vec::new() };
        for p in &places {
            // A greenness value with no word for where it came from renders as a measurement of this
            // city. The model gate refuses to emit one; this refuses to load one.
            match (p.ndvi, p.ndvi_basis.as_deref()) {
                (Some(_), Some("city")) | (Some(_), Some("country")) | (None, None) => {}
                (v, b) => return Err(format!(
                    "places.json: {}/{} has ndvi={v:?} with basis {b:?} — a value without its \
                     provenance, or the reverse; refusing this bundle", p.iso3, p.city)),
            }
            if !p.pm25.is_finite() || p.pm25 <= 0.0 {
                return Err(format!("places.json: {}/{} has pm25 {} — refusing", p.iso3, p.city, p.pm25));
            }
            if !(-90.0..=90.0).contains(&p.lat) || !(-180.0..=180.0).contains(&p.lon) {
                return Err(format!("places.json: {}/{} is not on Earth", p.iso3, p.city));
            }
        }

        // Scoreable must be a subset of drawable, or /api/meta offers a country the atlas cannot show.
        if !manifest.reference_countries.is_empty() {
            let drawable: std::collections::HashSet<&String> =
                manifest.reference_countries.iter().collect();
            let missing: Vec<&String> =
                manifest.countries.iter().filter(|c| !drawable.contains(c)).collect();
            if !missing.is_empty() {
                return Err(format!(
                    "manifest: scoreable countries {missing:?} are not in reference_countries"
                ));
            }
        }
        // An alias pointing nowhere is a country that used to work and now 400s.
        for (from, to) in &manifest.country_aliases {
            // Against the SCOREABLE map, because that is the only one `risk()` resolves through. An
            // alias pointing at a reference-only country would pass a check against both maps and
            // then 400 on every request — exactly the failure this refusal exists to prevent.
            if !baselines.contains_key(to) {
                return Err(format!(
                    "manifest: alias {from} -> {to} resolves to no SCOREABLE baseline"
                ));
            }
            if baselines.contains_key(from) || reference.contains_key(from) {
                return Err(format!("manifest: alias {from} shadows a real baseline"));
            }
        }
        Ok(Bundle { manifest, coefficients, baselines, reference, places, evidence, ontology })
    }
}
