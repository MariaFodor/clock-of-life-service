//! Scoring engine: a user profile -> Life-Clock estimate.
//!
//! Mirrors the witnessed Python (clock-of-life-model exp01/exp13): build the design vector, form the
//! linear predictor, centre it on the country's average person to get a relative risk, then apply that
//! risk to the national life table and integrate the survival curve into remaining years.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::bundle::{Baseline, Bundle, Coefficients};

/// Raw self-reported inputs (cohort-fitted features). Literature levers (diet/alcohol/…) join later.
#[derive(Deserialize, Serialize, Clone)]
pub struct Profile {
    pub country: String,
    pub age: f64,
    pub sex: String, // "M" | "F"
    pub smoke: u8,   // 0 never, 1 former, 2 current
    pub pa_min: f64, // weekly MET-minutes
    pub sleep: f64,  // hours
    pub waist: f64,  // cm
    #[serde(default)] pub bmi: Option<f64>,               // body-mass index (kg/m²), from height + weight
    #[serde(default)] pub cigs_day: f64,                  // current-smoker cigarettes/day (0 if not current)
    #[serde(default)] pub sbp: Option<f64>,               // systolic BP (mmHg) if known; else derived from high_bp
    #[serde(default)] pub diabetes: bool,
    #[serde(default)] pub high_bp: bool,
    #[serde(default)] pub respiratory: bool,
    #[serde(default)] pub cvd_hx: bool,
    #[serde(default)] pub cancer_hx: bool,
    #[serde(default)] pub higher_educ: bool,
    #[serde(default = "default_income")] pub income: f64, // income-to-poverty ratio
    #[serde(default)] pub pm25: Option<f64>, // home annual-mean PM2.5 µg/m³ (from location, RES-04)
    #[serde(default)] pub ndvi: Option<f64>, // home greenspace NDVI (from location)
    // Literature levers (LEV-02): unanswered (None) means "assume the average person" and
    // contributes exactly 0 to the linear predictor, preserving the national anchoring.
    #[serde(default)] pub diet_score: Option<f64>,    // Mediterranean-style item sum 0-5 (Q13-Q17)
    #[serde(default)] pub alcohol: Option<String>,    // none|light|moderate|heavy (Q18)
    #[serde(default)] pub sitting_hours: Option<f64>, // daily sitting/screen hours (Q10)
    #[serde(default)] pub stress_score: Option<f64>,  // PSS-4 sum 0-16 (Q19 a-d)
    #[serde(default)] pub mobility: Option<u8>,       // 0 none / 1 some / 2 a lot (Q22, fitted CONTEXT)
}

pub const ALCOHOL_LEVELS: &[&str] = &["none", "light", "moderate", "heavy"];
fn default_income() -> f64 { 2.5 }

// ENV term (RES-04): a location's log-hazard contribution vs the national-average reference. Illustrative
// RO reference — must be re-sourced with real RO PM2.5/NDVI layers before the relocation surface ships.
pub const RO_PM25_REF: f64 = 14.0;
pub const RO_NDVI_REF: f64 = 0.5;

/// The environment log-HR addend for a location: `ln(1.095)·(PM25−ref)/10 + ln(0.965)·(NDVI−ref)/0.1`.
/// Absent inputs contribute 0 (i.e. treated as the reference), so a location-less profile is unaffected.
pub fn env_term(pm25: Option<f64>, ndvi: Option<f64>) -> f64 {
    let air = pm25.map_or(0.0, |p| (1.095_f64).ln() * (p - RO_PM25_REF) / 10.0);
    let green = ndvi.map_or(0.0, |n| (0.965_f64).ln() * (n - RO_NDVI_REF) / 0.1);
    air + green
}

impl Profile {
    /// Reject out-of-range / nonsensical inputs so scoring can't emit NaN/inf or absurd year counts.
    pub fn validate(&self) -> Result<(), String> {
        let rng = |name: &str, v: f64, lo: f64, hi: f64| -> Result<(), String> {
            if v.is_finite() && (lo..=hi).contains(&v) { Ok(()) }
            else { Err(format!("{name} must be between {lo} and {hi}")) }
        };
        rng("age", self.age, 18.0, 110.0)?;
        if self.sex != "M" && self.sex != "F" {
            return Err("sex must be \"M\" or \"F\"".into());
        }
        if !(self.smoke <= 2) {
            return Err("smoke must be 0 (never), 1 (former), or 2 (current)".into());
        }
        rng("pa_min", self.pa_min, 0.0, 10_000.0)?;
        if !(self.sleep.is_finite() && self.sleep > 0.0 && self.sleep <= 24.0) {
            return Err("sleep must be between 0 and 24 hours".into());
        }
        rng("waist", self.waist, 40.0, 250.0)?;
        if let Some(bmi) = self.bmi {
            rng("bmi", bmi, 12.0, 70.0)?;
        }
        rng("cigs_day", self.cigs_day, 0.0, 80.0)?;
        if let Some(sbp) = self.sbp {
            rng("sbp", sbp, 70.0, 240.0)?;
        }
        rng("income", self.income, 0.0, 20.0)?;
        if let Some(pm25) = self.pm25 {
            rng("pm25", pm25, 0.0, 500.0)?;
        }
        if let Some(ndvi) = self.ndvi {
            rng("ndvi", ndvi, -1.0, 1.0)?;
        }
        if let Some(v) = self.diet_score {
            rng("diet_score", v, 0.0, 5.0)?;
        }
        if let Some(v) = self.sitting_hours {
            rng("sitting_hours", v, 0.0, 24.0)?;
        }
        if let Some(v) = self.stress_score {
            rng("stress_score", v, 0.0, 16.0)?;
        }
        if let Some(m) = self.mobility {
            if m > 2 {
                return Err("mobility must be 0 (none), 1 (some), or 2 (a lot)".into());
            }
        }
        if let Some(a) = self.alcohol.as_deref() {
            if !ALCOHOL_LEVELS.contains(&a) {
                return Err("alcohol must be one of none, light, moderate, heavy".into());
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub struct Estimate {
    pub estimate_years: f64,
    pub interval: [f64; 2],
    pub reaches_age: f64,
    pub relative_risk: f64,
    pub country: String,
}

/// Every design input that is z-scored against the bundle standardizer. `Bundle::load` validates
/// these keys exist, so the indexing in `z()` cannot panic on a served bundle.
pub const STANDARDIZED_KEYS: &[&str] = &["activity", "waist", "cigs_day", "bmi", "sbp", "income"];

fn z(raw: f64, coefs: &Coefficients, key: &str) -> f64 {
    let s = &coefs.standardizer[key];
    (raw - s.mean) / s.sd
}

/// The design vector, keyed by coefficient name (matches the bundle's coefficient keys).
pub fn design(p: &Profile, coefs: &Coefficients) -> HashMap<String, f64> {
    let young = if p.age < coefs.young_cutoff { 1.0 } else { 0.0 };
    let smk_current = if p.smoke == 2 { 1.0 } else { 0.0 };
    let activity = z((p.pa_min + 1.0).ln(), coefs, "activity");
    let waist = z(p.waist, coefs, "waist");
    let mut d = HashMap::new();
    d.insert("smk_former".into(), if p.smoke == 1 { 1.0 } else { 0.0 });
    d.insert("smk_current".into(), smk_current);
    // Current-smoker dose: 0 for never/former (matches the training encoding in config/features.py).
    let cigs = if p.smoke == 2 { p.cigs_day } else { 0.0 };
    d.insert("cigs_day".into(), z(cigs, coefs, "cigs_day"));
    // An omitted BMI must be neutral: the standardizer mean scores exactly z = 0. A hardcoded
    // constant (the old 27.0 vs the bundle mean 28.9) silently penalised every BMI-less request
    // by RR ×1.33 (REVIEW-2026-09-09 S2).
    let bmi = p.bmi.unwrap_or(coefs.standardizer["bmi"].mean);
    d.insert("bmi".into(), z(bmi, coefs, "bmi"));
    // Systolic BP: a real reading when known; otherwise derived from the high-BP answer
    // (NHANES hbp-conditional means), so the field refines rather than duplicates high_bp.
    let sbp = p.sbp.unwrap_or(if p.high_bp { 132.7 } else { 117.9 });
    d.insert("sbp".into(), z(sbp, coefs, "sbp"));
    d.insert("activity".into(), activity);
    d.insert("sleep_long".into(), if p.sleep >= 8.5 { 1.0 } else { 0.0 });
    d.insert("waist".into(), waist);
    d.insert("diabetes".into(), b(p.diabetes));
    d.insert("high_bp".into(), b(p.high_bp));
    d.insert("respiratory".into(), b(p.respiratory));
    // The questionnaire collects mobility as ordinal 0/1/2, but the coefficient was fitted on the
    // BINARY any-difficulty encoding (harmonize.py: pfq_diff = PFQ061B > 1), so "some" and "a lot"
    // both score 1 — scoring 2 would extrapolate to 2β, which the fit never estimated (PR#1 F1).
    // An ordinal refit is a model task (Bundle 8).
    d.insert("mobility".into(), p.mobility.map_or(0.0, |m| if m > 0 { 1.0 } else { 0.0 }));
    d.insert("cvd_hx".into(), b(p.cvd_hx));
    d.insert("cancer_hx".into(), b(p.cancer_hx));
    d.insert("education".into(), b(p.higher_educ));
    d.insert("income".into(), z(p.income, coefs, "income"));
    d.insert("smk_current_x_young".into(), smk_current * young);
    d.insert("activity_x_young".into(), activity * young);
    d.insert("waist_x_young".into(), waist * young);
    d
}
fn b(v: bool) -> f64 { if v { 1.0 } else { 0.0 } }

pub fn linear_predictor(design: &HashMap<String, f64>, coefs: &HashMap<String, f64>) -> f64 {
    coefs.iter().map(|(k, c)| c * design.get(k).copied().unwrap_or(0.0)).sum()
}

/// Remaining life expectancy from `start_age` given a relative-risk multiplier on the baseline hazard.
/// `qx` maps age (as string) -> yearly probability of death; missing ages are linearly interpolated.
pub fn remaining_le(qx: &HashMap<String, f64>, start_age: i64, rr: f64) -> f64 {
    let mut pts: Vec<(i64, f64)> = qx.iter().filter_map(|(k, v)| k.parse::<i64>().ok().map(|a| (a, *v))).collect();
    pts.sort_by_key(|p| p.0);
    let (lo, hi) = (pts[0].0, pts[pts.len() - 1].0);
    // dense per-age table with linear gap fill
    let mut dense = vec![0.0_f64; (hi - lo + 1) as usize];
    let mut j = 0;
    for age in lo..=hi {
        if pts[j].0 == age {
            dense[(age - lo) as usize] = pts[j].1;
            if j + 1 < pts.len() { j += 1; }
        } else {
            let (pa, pv) = pts[j - 1];
            let (na, nv) = pts[j];
            let t = (age - pa) as f64 / (na - pa) as f64;
            dense[(age - lo) as usize] = pv + t * (nv - pv);
        }
    }
    let q_at = |age: i64| dense[((age.min(hi)) - lo).max(0) as usize];
    let (mut s, mut le) = (1.0_f64, 0.0_f64);
    for age in start_age..=110 {
        let q = q_at(age);
        let qa = 1.0 - (1.0 - q).powf(rr);
        le += s * (1.0 - qa / 2.0);
        s *= 1.0 - qa;
    }
    le
}

/// The literature levers' log-hazard contribution. Every term is a DEVIATION from its centring
/// reference (bundle `literature[..].reference`): an unanswered lever or an average answer adds
/// exactly 0, so the average person still reads RR 1.0 against the national life table (EXP-13).
pub fn literature_lp(p: &Profile, coefs: &Coefficients) -> f64 {
    let mut lp = 0.0;
    let cont = [
        ("diet", p.diet_score),
        ("sedentary", p.sitting_hours),
        ("stress", p.stress_score),
    ];
    for (key, answer) in cont {
        if let (Some(f), Some(raw)) = (coefs.literature.get(key), answer) {
            if let Some(beta) = f.beta {
                // reference kind "mean" => z(reference) = 0, so the deviation is just beta * z(user).
                lp += beta * z(raw, coefs, key);
            }
        }
    }
    if let (Some(f), Some(level)) = (coefs.literature.get("alcohol"), p.alcohol.as_deref()) {
        if let Some(levels) = f.levels.as_ref() {
            // The load gate guarantees: all ALCOHOL_LEVELS present, reference declared and present —
            // so these lookups cannot silently misprice a level (PR#1 F3).
            let reference = f.reference.as_ref().and_then(|r| r.level.as_deref()).unwrap_or("none");
            lp += levels.get(level).copied().unwrap_or(0.0)
                - levels.get(reference).copied().unwrap_or(0.0);
        }
    }
    lp
}

/// Precise relative risk for a profile, plus the resolved country baseline.
fn risk<'a>(bundle: &'a Bundle, p: &Profile) -> Result<(f64, &'a Baseline), String> {
    let base = bundle.baselines.get(&p.country)
        .ok_or_else(|| format!("no baseline for country {}", p.country))?;
    // Cohort-fitted linear predictor + the location ENV term (context; 0 for a location-less profile,
    // and 0 for an average-location user, so the national-average reference is unaffected).
    let lp = linear_predictor(&design(p, &bundle.coefficients), &bundle.coefficients.prediction)
        + env_term(p.pm25, p.ndvi)
        + literature_lp(p, &bundle.coefficients);
    let reference = if p.age < bundle.coefficients.young_cutoff { base.reference_lp.young } else { base.reference_lp.old };
    Ok(((lp - reference).exp(), base))
}

pub fn estimate(bundle: &Bundle, p: &Profile) -> Result<Estimate, String> {
    p.validate()?;
    let (rr, base) = risk(bundle, p)?;
    let qx = base.qx.get(&p.sex).ok_or_else(|| format!("no qx for sex {}", p.sex))?;

    let years = remaining_le(qx, p.age.round() as i64, rr);
    let rel = if p.age >= 55.0 { 0.06 } else { 0.10 }; // interval widens for the young (sparse deaths) — heuristic v1
    Ok(Estimate {
        estimate_years: round1(years),
        interval: [round1(years * (1.0 - rel)), round1(years * (1.0 + rel))],
        reaches_age: round1(p.age + years),
        relative_risk: (rr * 1000.0).round() / 1000.0,
        country: p.country.clone(),
    })
}

fn round1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }

/// One factor's contribution to the estimate, for the "Why?" surface.
#[derive(Serialize)]
pub struct Attribution {
    /// The feature/design key (e.g. "smk_current"), so callers can map back to features/rules.
    pub key: String,
    pub factor: String,
    /// Years this factor adds (+) or costs (-) vs its reference level (cohort mean for z-scored
    /// continuous factors, or the factor being absent for binary factors).
    pub delta_years: f64,
    pub role: String,
    pub evidence: String, // evidence grade
    pub citation: String,
}

/// Main-effect design keys (age-interaction `*_x_young` terms excluded) with user-facing labels.
/// `mobility` fires only when the caller supplies it; unanswered profiles score 0.
const FACTORS: &[(&str, &str)] = &[
    ("smk_former", "Former smoking"),
    ("smk_current", "Current smoking"),
    ("activity", "Physical activity"),
    ("sleep_long", "Long sleep"),
    ("waist", "Waist circumference"),
    ("diabetes", "Diabetes"),
    ("high_bp", "High blood pressure"),
    ("respiratory", "Respiratory disease"),
    ("mobility", "Mobility limitation"),
    ("cvd_hx", "Cardiovascular history"),
    ("cancer_hx", "Cancer history"),
    ("education", "Education"),
    ("income", "Income"),
];

/// Per-factor "Why?" attribution: for each factor the user deviates from the reference on, the year
/// delta of removing that factor's contribution. Levers use the TOTAL-EFFECT (attribution)
/// coefficients so they read honestly; manage/context factors (no attribution term) fall back to the
/// fitted prediction coefficient. Main effects only — the `*_x_young` terms are excluded (EXP-01).
pub fn attributions(bundle: &Bundle, p: &Profile) -> Result<Vec<Attribution>, String> {
    p.validate()?;
    let (base_rr, base) = risk(bundle, p)?;
    let qx = base.qx.get(&p.sex).ok_or_else(|| format!("no qx for sex {}", p.sex))?;
    let age = p.age.round() as i64;
    let years_actual = remaining_le(qx, age, base_rr);
    let d0 = design(p, &bundle.coefficients);
    let attr = &bundle.coefficients.attribution;

    let mut out = Vec::new();
    for (key, label) in FACTORS {
        let x = d0.get(*key).copied().unwrap_or(0.0);
        if x == 0.0 {
            continue; // no deviation from the reference on this factor
        }
        // Total-effect coefficient for the levers; fitted (prediction) coefficient for the
        // manage/context factors, which have no separate attribution term.
        let c = match attr.get(*key).or_else(|| bundle.coefficients.prediction.get(*key)) {
            Some(c) => *c,
            None => continue,
        };
        let d_lp = c * x;
        let rr_without = base_rr * (-d_lp).exp(); // remove this factor's contribution
        let years_without = remaining_le(qx, age, rr_without);
        let delta = years_actual - years_without;
        if delta.abs() < 0.05 {
            continue; // negligible — don't clutter the Why? list
        }
        let ev = bundle.evidence.get(*key);
        out.push(Attribution {
            key: (*key).to_string(),
            factor: label.to_string(),
            delta_years: round1(delta),
            role: ev.map(|e| e.role.clone()).unwrap_or_default(),
            evidence: ev.map(|e| e.grade.clone()).unwrap_or_default(),
            citation: ev.map(|e| e.citation.clone()).unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| {
        b.delta_years
            .abs()
            .partial_cmp(&a.delta_years.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// A lifestyle change to explore. Only modifiable levers may change (manage/context/baseline are fixed).
#[derive(Deserialize, Serialize)]
pub struct WhatIfChanges {
    pub smoke: Option<u8>,
    pub pa_min: Option<f64>,
    pub sleep: Option<f64>,
    pub waist: Option<f64>,
}

#[derive(Serialize)]
pub struct WhatIf {
    pub current_years: f64,
    pub scenario_years: f64,
    pub delta_years: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Non-persisted overlay: apply lever changes and report the change in remaining years.
/// The delta uses the TOTAL-EFFECT attribution model (not prediction), so waist/smoking read honestly.
pub fn whatif(bundle: &Bundle, base: &Profile, changes: &WhatIfChanges) -> Result<WhatIf, String> {
    base.validate()?;
    let (base_rr, baseline) = risk(bundle, base)?;
    let qx = baseline.qx.get(&base.sex).ok_or_else(|| format!("no qx for sex {}", base.sex))?;
    let age = base.age.round() as i64;
    let current_years = remaining_le(qx, age, base_rr);

    let mut modified = base.clone();
    let mut note = None;
    if let Some(v) = changes.smoke {
        // Quitting: the cohort's former-smoker coefficient reflects reverse causation ("sick-quitter"),
        // so it must NOT drive the causal What-If. Model reducing smoking as trending toward never-smoker
        // risk (the causal target); the real benefit accrues over ~10 years (cessation detail is a
        // deferred model refinement, RES-02). Increasing smoking is taken at face value.
        if v < base.smoke {
            modified.smoke = 0;
            note = Some("smoking-cessation benefit accrues over ~10 years; shown as the long-run effect".into());
        } else {
            modified.smoke = v;
        }
    }
    if let Some(v) = changes.pa_min { modified.pa_min = v; }
    if let Some(v) = changes.sleep { modified.sleep = v; }
    if let Some(v) = changes.waist { modified.waist = v; }
    modified.validate().map_err(|e| format!("change produces invalid profile: {e}"))?;

    // Delta uses the total-effect attribution MAIN effects only. The age-interaction terms
    // (*_x_young) are mis-specified (EXP-01) and corrupt marginal What-If deltas, so they are excluded
    // here; age still enters the base estimate via the life table. (Age-stratified coefficients are the
    // proper fix, deferred to model v2.1.)
    let attr = &bundle.coefficients.attribution;
    let d0 = design(base, &bundle.coefficients);
    let d1 = design(&modified, &bundle.coefficients);
    let d_lp: f64 = attr.iter()
        .filter(|(k, _)| !k.ends_with("_x_young"))
        .map(|(k, c)| c * (d1.get(k).copied().unwrap_or(0.0) - d0.get(k).copied().unwrap_or(0.0)))
        .sum();
    let scenario_rr = base_rr * d_lp.exp();
    let scenario_years = remaining_le(qx, age, scenario_rr);

    Ok(WhatIf {
        current_years: round1(current_years),
        scenario_years: round1(scenario_years),
        delta_years: round1(scenario_years - current_years),
        note,
    })
}

/// Read a numeric view of a `Profile` field by name (booleans as 0.0/1.0). None for unknown fields.
fn profile_field(p: &Profile, field: &str) -> Option<f64> {
    Some(match field {
        "age" => p.age,
        "smoke" => p.smoke as f64,
        "pa_min" => p.pa_min,
        "sleep" => p.sleep,
        "waist" => p.waist,
        "income" => p.income,
        "diabetes" => b(p.diabetes),
        "high_bp" => b(p.high_bp),
        "respiratory" => b(p.respiratory),
        "cvd_hx" => b(p.cvd_hx),
        "cancer_hx" => b(p.cancer_hx),
        "higher_educ" => b(p.higher_educ),
        _ => return None,
    })
}

/// Evaluate a `recommendation_rule.condition` (`{field, op, value}`) against a profile.
/// Supported ops: eq, ne, gt, lt, gte, lte. Booleans compare as 1.0/0.0. Malformed → false.
pub fn eval_condition(p: &Profile, condition: &serde_json::Value) -> bool {
    let field = condition.get("field").and_then(|v| v.as_str());
    let op = condition.get("op").and_then(|v| v.as_str());
    let value = condition.get("value").and_then(|v| {
        v.as_f64().or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
    });
    let (field, op, rhs) = match (field, op, value) {
        (Some(f), Some(o), Some(v)) => (f, o, v),
        _ => return false,
    };
    let lhs = match profile_field(p, field) {
        Some(x) => x,
        None => return false,
    };
    match op {
        "eq" => (lhs - rhs).abs() < 1e-9,
        "ne" => (lhs - rhs).abs() >= 1e-9,
        "gt" => lhs > rhs,
        "lt" => lhs < rhs,
        "gte" => lhs >= rhs,
        "lte" => lhs <= rhs,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn bundle() -> Bundle {
        Bundle::load(Path::new("bundle/model-v2.2.0")).expect("bundle loads")
    }

    #[test]
    fn omitted_bmi_is_neutral() {
        // A profile without BMI must score z = 0 on the bmi term — the standardizer mean, not a
        // hardcoded constant (the old 27.0 default silently added RR ×1.33; REVIEW-2026-09-09 S2).
        let b = bundle();
        let p = Profile {
            country: "RO".into(), age: 40.0, sex: "M".into(), smoke: 0, pa_min: 600.0, sleep: 7.0,
            waist: 95.0, bmi: None, cigs_day: 0.0, sbp: None, diabetes: false, high_bp: false,
            respiratory: false, cvd_hx: false, cancer_hx: false, higher_educ: false, income: 2.5,
            pm25: None, ndvi: None, diet_score: None, alcohol: None, sitting_hours: None,
            stress_score: None, mobility: None,
        };
        let d = design(&p, &b.coefficients);
        assert!(d["bmi"].abs() < 1e-12, "omitted bmi must be exactly neutral, got z = {}", d["bmi"]);
    }

    fn plain_profile() -> Profile {
        Profile {
            country: "RO".into(), age: 50.0, sex: "M".into(), smoke: 0, pa_min: 600.0, sleep: 7.0,
            waist: 95.0, bmi: None, cigs_day: 0.0, sbp: None, diabetes: false, high_bp: false,
            respiratory: false, cvd_hx: false, cancer_hx: false, higher_educ: false, income: 2.5,
            pm25: None, ndvi: None, diet_score: None, alcohol: None, sitting_hours: None,
            stress_score: None, mobility: None,
        }
    }

    #[test]
    fn literature_levers_neutral_at_reference() {
        // Answering exactly at each lever's centring reference must contribute 0 log-hazard —
        // identical to not answering at all — so the national anchoring survives LEV-02.
        let b = bundle();
        let mut p = plain_profile();
        assert!(literature_lp(&p, &b.coefficients).abs() < 1e-12, "unanswered levers must be 0");
        p.diet_score = Some(2.5);      // the shipped standardizer means
        p.sitting_hours = Some(6.0);
        p.stress_score = Some(6.11);
        p.alcohol = Some("light".into()); // the shipped centring reference level
        assert!(literature_lp(&p, &b.coefficients).abs() < 1e-9,
                "reference answers must be neutral, got {}", literature_lp(&p, &b.coefficients));
    }

    #[test]
    fn literature_levers_move_the_estimate_directionally() {
        let b = bundle();
        let years = |f: &dyn Fn(&mut Profile)| {
            let mut p = plain_profile();
            f(&mut p);
            estimate(&b, &p).unwrap().estimate_years
        };
        let base = years(&|_| {});
        assert!(years(&|p| p.alcohol = Some("heavy".into())) < years(&|p| p.alcohol = Some("none".into())),
                "heavy drinking must cost more than abstaining");
        assert!(years(&|p| p.diet_score = Some(5.0)) > years(&|p| p.diet_score = Some(0.0)),
                "a better diet must read better than a worse one");
        assert!(years(&|p| p.sitting_hours = Some(12.0)) < base, "heavy sitting must cost years");
        assert!(years(&|p| p.stress_score = Some(16.0)) < base, "max stress must cost years");
        assert!(years(&|p| p.mobility = Some(1)) < base,
                "any mobility difficulty must lower the estimate (fitted CONTEXT beta, was hardcoded 0)");
        // The coefficient is fitted on the BINARY any-difficulty encoding: "some" and "a lot"
        // must price identically until the ordinal refit (PR#1 F1 pin).
        assert_eq!(years(&|p| p.mobility = Some(1)), years(&|p| p.mobility = Some(2)),
                "binary encoding: mobility 1 and 2 score the same");
    }

    #[test]
    fn avg_person_matches_national_life_expectancy() {
        // By construction the average person has LP == reference_lp, so RR == 1 and remaining years
        // must equal the national life-table figure the bundle stored.
        let b = bundle();
        let ro = &b.baselines["RO"];
        for sex in ["M", "F"] {
            let le = remaining_le(&ro.qx[sex], 40, 1.0);
            let national = ro.national_le_40[sex];
            assert!((le - national).abs() < 0.1, "{sex}: {le} vs national {national}");
        }
    }

    #[test]
    fn healthy_outlives_high_risk() {
        let b = bundle();
        let base = |smoke, pa, waist, diab| Profile {
            country: "RO".into(), age: 40.0, sex: "M".into(), smoke, pa_min: pa, sleep: 7.0,
            waist, bmi: Some(27.0), cigs_day: 0.0, sbp: None, diabetes: diab, high_bp: diab, respiratory: false, cvd_hx: false, cancer_hx: false,
            higher_educ: true, income: 4.0, pm25: None, ndvi: None, diet_score: None, alcohol: None, sitting_hours: None,
            stress_score: None, mobility: None,
        };
        let healthy = estimate(&b, &base(0, 2000.0, 85.0, false)).unwrap();
        let high = estimate(&b, &base(2, 0.0, 115.0, true)).unwrap();
        // Robust ordering: the healthy profile outlives and out-ranks the high-risk one. (Absolute RR
        // depends on SES + the paradoxical adjusted-waist coefficient, so we assert the ordering, not a
        // fixed threshold — What-If uses the total-effect attribution model instead of prediction.)
        assert!(healthy.estimate_years > high.estimate_years + 5.0,
                "healthy {} should exceed high-risk {} by years", healthy.estimate_years, high.estimate_years);
        assert!(healthy.relative_risk < high.relative_risk,
                "healthy rr {} should be below high-risk rr {}", healthy.relative_risk, high.relative_risk);
        assert!(healthy.relative_risk < 1.0, "healthy profile should be below-average risk");
    }

    #[test]
    fn rejects_bad_input() {
        let b = bundle();
        let ok = Profile { country: "RO".into(), age: 40.0, sex: "M".into(), smoke: 0, pa_min: 300.0,
            sleep: 7.0, waist: 90.0, bmi: Some(27.0), cigs_day: 0.0, sbp: None, diabetes: false, high_bp: false, respiratory: false, cvd_hx: false,
            cancer_hx: false, higher_educ: false, income: 2.5, pm25: None, ndvi: None,
            diet_score: None, alcohol: None, sitting_hours: None, stress_score: None, mobility: None };
        assert!(estimate(&b, &ok).is_ok());
        let bad = |f: &dyn Fn(&mut Profile)| { let mut p = ok.clone(); f(&mut p); estimate(&b, &p).is_err() };
        assert!(bad(&|p| p.pa_min = -5.0), "negative activity rejected");
        assert!(bad(&|p| p.age = 5.0), "child age rejected");
        assert!(bad(&|p| p.waist = 5.0), "implausible waist rejected");
        assert!(bad(&|p| p.sex = "X".into()), "bad sex rejected");
        assert!(bad(&|p| p.sleep = 30.0), "impossible sleep rejected");
    }

    #[test]
    fn env_term_signs_and_neutrality() {
        assert_eq!(env_term(None, None), 0.0, "no location → no ENV effect");
        assert!(env_term(Some(RO_PM25_REF), Some(RO_NDVI_REF)).abs() < 1e-12, "reference → ENV 0");
        assert!(env_term(Some(24.0), Some(RO_NDVI_REF)) > 0.0, "dirtier air → positive log-HR (worse)");
        assert!(env_term(Some(RO_PM25_REF), Some(0.7)) < 0.0, "greener → negative log-HR (better)");
    }

    #[test]
    fn cleaner_location_outlives_polluted() {
        let b = bundle();
        let at = |pm25, ndvi| Profile {
            country: "RO".into(), age: 45.0, sex: "M".into(), smoke: 0, pa_min: 600.0, sleep: 7.0,
            waist: 90.0, bmi: Some(27.0), cigs_day: 0.0, sbp: None, diabetes: false, high_bp: false, respiratory: false, cvd_hx: false,
            cancer_hx: false, higher_educ: false, income: 2.5,
            pm25: Some(pm25), ndvi: Some(ndvi), diet_score: None, alcohol: None,
            sitting_hours: None, stress_score: None, mobility: None,
        };
        let polluted = estimate(&b, &at(19.0, 0.35)).unwrap(); // Bucharest-like
        let clean = estimate(&b, &at(10.0, 0.70)).unwrap();    // rural-like
        assert!(clean.estimate_years > polluted.estimate_years, "cleaner air + greener → more years");
        assert!(polluted.relative_risk > clean.relative_risk);
    }

    #[test]
    fn condition_evaluation() {
        let p = Profile {
            country: "RO".into(), age: 55.0, sex: "M".into(), smoke: 2, pa_min: 100.0, sleep: 9.0,
            waist: 110.0, bmi: Some(30.0), cigs_day: 20.0, sbp: Some(150.0), diabetes: true, high_bp: false, respiratory: false, cvd_hx: false,
            cancer_hx: false, higher_educ: false, income: 2.5, pm25: None, ndvi: None, diet_score: None, alcohol: None, sitting_hours: None,
            stress_score: None, mobility: None,
        };
        use serde_json::json;
        assert!(eval_condition(&p, &json!({"field": "smoke", "op": "eq", "value": 2})));
        assert!(!eval_condition(&p, &json!({"field": "smoke", "op": "eq", "value": 0})));
        assert!(eval_condition(&p, &json!({"field": "pa_min", "op": "lt", "value": 500})));
        assert!(eval_condition(&p, &json!({"field": "waist", "op": "gt", "value": 100})));
        assert!(eval_condition(&p, &json!({"field": "sleep", "op": "gte", "value": 9})));
        assert!(eval_condition(&p, &json!({"field": "diabetes", "op": "eq", "value": true})));
        assert!(!eval_condition(&p, &json!({"field": "high_bp", "op": "eq", "value": true})));
        // Malformed / unknown → false, never panics.
        assert!(!eval_condition(&p, &json!({"field": "nonsense", "op": "eq", "value": 1})));
        assert!(!eval_condition(&p, &json!({"op": "eq", "value": 1})));
    }

    #[test]
    fn attributions_explain_a_high_risk_profile() {
        let b = bundle();
        let p = Profile {
            country: "RO".into(), age: 55.0, sex: "M".into(), smoke: 2, pa_min: 0.0, sleep: 7.0,
            waist: 115.0, bmi: Some(32.0), cigs_day: 20.0, sbp: Some(160.0), diabetes: true, high_bp: true, respiratory: false, cvd_hx: false,
            cancer_hx: false, higher_educ: true, income: 4.0, pm25: None, ndvi: None, diet_score: None, alcohol: None, sitting_hours: None,
            stress_score: None, mobility: None,
        };
        let why = attributions(&b, &p).unwrap();
        assert!(!why.is_empty(), "a high-risk profile should have explanatory factors");

        // Sorted by descending magnitude.
        for pair in why.windows(2) {
            assert!(pair[0].delta_years.abs() >= pair[1].delta_years.abs(), "sorted by |delta|");
        }
        // Manage/context factors appear too (prediction-coefficient fallback), not only levers.
        let smoking = why.iter().find(|a| a.factor == "Current smoking").expect("smoking present");
        assert!(smoking.delta_years < 0.0, "current smoking costs years");
        assert_eq!(smoking.evidence, "strong");
        assert!(why.iter().any(|a| a.factor == "Diabetes" && a.delta_years < 0.0), "diabetes explained");
        // A protective factor reads positive.
        assert!(why.iter().any(|a| a.factor == "Education" && a.delta_years > 0.0), "education adds years");
    }
}
