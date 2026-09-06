//! Scoring engine: a user profile -> Life-Clock estimate.
//!
//! Mirrors the witnessed Python (clock-of-life-model exp01/exp13): build the design vector, form the
//! linear predictor, centre it on the country's average person to get a relative risk, then apply that
//! risk to the national life table and integrate the survival curve into remaining years.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::bundle::{Baseline, Bundle, Coefficients};

/// Raw self-reported inputs (cohort-fitted features). Literature levers (diet/alcohol/…) join later.
#[derive(Deserialize, Clone)]
pub struct Profile {
    pub country: String,
    pub age: f64,
    pub sex: String, // "M" | "F"
    pub smoke: u8,   // 0 never, 1 former, 2 current
    pub pa_min: f64, // weekly MET-minutes
    pub sleep: f64,  // hours
    pub waist: f64,  // cm
    #[serde(default)] pub diabetes: bool,
    #[serde(default)] pub high_bp: bool,
    #[serde(default)] pub respiratory: bool,
    #[serde(default)] pub cvd_hx: bool,
    #[serde(default)] pub cancer_hx: bool,
    #[serde(default)] pub higher_educ: bool,
    #[serde(default = "default_income")] pub income: f64, // income-to-poverty ratio
}
fn default_income() -> f64 { 2.5 }

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
        rng("income", self.income, 0.0, 20.0)?;
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
    d.insert("activity".into(), activity);
    d.insert("sleep_long".into(), if p.sleep >= 8.5 { 1.0 } else { 0.0 });
    d.insert("waist".into(), waist);
    d.insert("diabetes".into(), b(p.diabetes));
    d.insert("high_bp".into(), b(p.high_bp));
    d.insert("respiratory".into(), b(p.respiratory));
    d.insert("mobility".into(), 0.0); // asked separately; default none for v1 estimate
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

/// Precise relative risk for a profile, plus the resolved country baseline.
fn risk<'a>(bundle: &'a Bundle, p: &Profile) -> Result<(f64, &'a Baseline), String> {
    let base = bundle.baselines.get(&p.country)
        .ok_or_else(|| format!("no baseline for country {}", p.country))?;
    let lp = linear_predictor(&design(p, &bundle.coefficients), &bundle.coefficients.prediction);
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

/// A lifestyle change to explore. Only modifiable levers may change (manage/context/baseline are fixed).
#[derive(Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn bundle() -> Bundle {
        Bundle::load(Path::new("bundle/model-v2.0.0")).expect("bundle loads")
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
            waist, diabetes: diab, high_bp: diab, respiratory: false, cvd_hx: false, cancer_hx: false,
            higher_educ: true, income: 4.0,
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
            sleep: 7.0, waist: 90.0, diabetes: false, high_bp: false, respiratory: false, cvd_hx: false,
            cancer_hx: false, higher_educ: false, income: 2.5 };
        assert!(estimate(&b, &ok).is_ok());
        let bad = |f: &dyn Fn(&mut Profile)| { let mut p = ok.clone(); f(&mut p); estimate(&b, &p).is_err() };
        assert!(bad(&|p| p.pa_min = -5.0), "negative activity rejected");
        assert!(bad(&|p| p.age = 5.0), "child age rejected");
        assert!(bad(&|p| p.waist = 5.0), "implausible waist rejected");
        assert!(bad(&|p| p.sex = "X".into()), "bad sex rejected");
        assert!(bad(&|p| p.sleep = 30.0), "impossible sleep rejected");
    }
}
