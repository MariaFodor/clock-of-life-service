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

pub fn estimate(bundle: &Bundle, p: &Profile) -> Result<Estimate, String> {
    let base: &Baseline = bundle.baselines.get(&p.country)
        .ok_or_else(|| format!("no baseline for country {}", p.country))?;
    let qx = base.qx.get(&p.sex).ok_or_else(|| format!("no qx for sex {}", p.sex))?;

    let d = design(p, &bundle.coefficients);
    let lp = linear_predictor(&d, &bundle.coefficients.prediction);
    let reference = if p.age < bundle.coefficients.young_cutoff { base.reference_lp.young } else { base.reference_lp.old };
    let rr = (lp - reference).exp();

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
}
