# Model card — The Clock of Life v3.0.0

**Algorithm:** Cox proportional hazards (interpretable), lifestyle + pathology predictors; age & sex to
the national life-table baseline. **Attribution/What-If** uses a separate total-effect model.

**Training:** NHANES 2007-2014 linked to NCHS mortality (through 2019-12-31); n=18839,
deaths=2119. **Discrimination** C-index 0.76; **calibration** MAE
0.013.

**Fitting (v3.0.0+):** stratified by age band x sex — each stratum has its own baseline hazard, so
coefficients compare people of the same age and sex, and age stays out of the linear predictor.
The C-index above is therefore GLOBAL and not comparable to earlier unstratified versions (v2.2.0
read 0.805): the old figure was inflated by age confounding leaking into the lever coefficients.
Discrimination between people of the same age and sex, which is what the product actually does,
improved. Sign constraints from the ontology are enforced as optimizer bounds; coefficients listed
in `coefficients.clipped_at_bound` were pinned there by a declared decision, not measured.
Per-lever TOTAL effects are fitted on the adjustment set the causal graph implies and, where the
prior is on the same scale, precision-weighted against it; `total_effect_data_only` records what
the cohort alone said.

**Baselines:** 30 countries (Eurostat life tables); relative risk centred on each country's
average person (smoking & weight from EHIS, cohort-mean fallback otherwise).

**Literature features** (diet, alcohol, sedentary, stress, environment) are appended from the evidence
base at stated confidence, not fitted on the cohort.

**Intended use:** wellness/education only — a statistical estimate, never a prediction or diagnosis
(ADR-001). Recommendations target modifiable/manageable factors only.
