# Model card — The Clock of Life v4.1.1

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

**Baselines:** life tables for 237 countries or areas (UN World Population Prospects 2024,
2023 estimates, single year of age 0-100 by sex; CC BY 3.0 IGO). Of those, **30 can be
SCORED** — relative risk is centred on the country's average person using smoking and weight from
Eurostat EHIS, and only those countries have it. The rest carry a life table so the atlas can draw
them and are refused a personal estimate. Eurostat life tables (2024) are retained as the
independent cross-source witness the release gates check against, not as a source.

**Literature features** (diet, alcohol, sedentary, stress, environment) are appended from the evidence
base at stated confidence, not fitted on the cohort.

**Exposure reference (v4.1.0+):** each baseline carries `env_reference` — the measured air and greenness
an average person in that country is exposed to, which is what the environment term is CENTRED on. Air:
WHO Global Health Observatory `SDGPM25`, population-weighted and split by residence area (total / urban /
rural / city / town), so a reader in a village is not centred on a capital-city average. Greenness:
population-weighted annual mean NDVI from Stowell et al. 2023 (CC0), derived from that country's own
measured cities, with `ndvi_cities` recording how many — 22 of the 30 scoreable countries rest on ONE
city, and the figure must be labelled as that rather than presented as a measurement of the country.
`places.json` carries 3,521 real settlements in 85 countries, each with its own
PM2.5 reading and `ndvi_basis` saying whether its greenness is its own or its country's.

**Inherited licence:** WHO's air data is CC BY-NC-SA 3.0 IGO — non-commercial and SHARE-ALIKE, and that
obligation attaches to this bundle and to anything distributed containing it. `manifest.licences` states
it so it travels with the artifact.

**Intended use:** wellness/education only — a statistical estimate, never a prediction or diagnosis
(ADR-001). Recommendations target modifiable/manageable factors only.
