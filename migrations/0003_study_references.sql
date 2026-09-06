-- Structured evidence store: turn free-text citations into openable studies with DOIs/URLs, linked
-- many-to-many to the features and recommendation rules they back. A `study` row corresponds to a
-- reviewed article (an analysed_papers/ review, via `review_slug`, which itself carries the DOI).

CREATE TABLE study (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code           TEXT NOT NULL UNIQUE,           -- stable slug (= analysed_papers filename)
    title          TEXT NOT NULL,
    authors        TEXT,
    year           INT,
    venue          TEXT,
    doi            TEXT,                            -- resolvable DOI, when the study is a single paper
    url            TEXT,                            -- e.g. https://doi.org/<doi>
    review_slug    TEXT,                            -- analysed_papers/<slug>.md (the internal review)
    evidence_grade TEXT CHECK (evidence_grade IN ('strong','moderate','weak','na')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Many-to-many: one paper backs several factors; one factor cites several papers.
CREATE TABLE feature_study (
    feature_key TEXT NOT NULL REFERENCES feature(key) ON DELETE CASCADE,
    study_code  TEXT NOT NULL REFERENCES study(code) ON DELETE CASCADE,
    PRIMARY KEY (feature_key, study_code)
);

CREATE TABLE rule_study (
    rule_code   TEXT NOT NULL REFERENCES recommendation_rule(code) ON DELETE CASCADE,
    study_code  TEXT NOT NULL REFERENCES study(code) ON DELETE CASCADE,
    PRIMARY KEY (rule_code, study_code)
);
