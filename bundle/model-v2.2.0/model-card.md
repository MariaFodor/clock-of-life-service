# Model card — The Clock of Life v2.2.0

**Algorithm:** Cox proportional hazards (interpretable), lifestyle + pathology predictors; age & sex to
the national life-table baseline. **Attribution/What-If** uses a separate total-effect model.

**Training:** NHANES 2007-2014 linked to NCHS mortality (through 2019-12-31); n=18839,
deaths=2119. **Discrimination** C-index 0.82; **calibration** MAE
0.019.

**Baselines:** 30 countries (Eurostat life tables); relative risk centred on each country's
average person (smoking & weight from EHIS, cohort-mean fallback otherwise).

**Literature features** (diet, alcohol, sedentary, stress, environment) are appended from the evidence
base at stated confidence, not fitted on the cohort.

**Intended use:** wellness/education only — a statistical estimate, never a prediction or diagnosis
(ADR-001). Recommendations target modifiable/manageable factors only.
