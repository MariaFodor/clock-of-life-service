-- The Clock of Life — application database (full 11-table schema).
-- Mirrors architecture/service/db-schema.md. UUID PKs, timestamptz, JSONB payloads, snake_case.
-- gen_random_uuid() is core in PostgreSQL 13+ (no pgcrypto extension needed).

CREATE TABLE account (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email_hash      TEXT NOT NULL UNIQUE,              -- login only; never used for contact (ADR-002)
    password_hash   TEXT NOT NULL,                     -- argon2
    locale          TEXT NOT NULL DEFAULT 'ro',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_active_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE location (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL,
    country    TEXT NOT NULL DEFAULT 'RO',
    pm25       NUMERIC,                                -- annual mean µg/m³ (RES-04)
    ndvi       NUMERIC,                                -- greenspace index
    area_type  TEXT,                                   -- city | suburb | rural
    as_of      DATE,
    UNIQUE (name, country)
);

CREATE TABLE profile (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id        UUID NOT NULL UNIQUE REFERENCES account(id) ON DELETE CASCADE,
    home_location_id  UUID REFERENCES location(id),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE feature (
    key             TEXT PRIMARY KEY,                  -- e.g. 'activity','smk_current','waist','diabetes','env'
    name            TEXT NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('lever','manage','context','baseline')),
    evidence_grade  TEXT CHECK (evidence_grade IN ('strong','moderate','weak','na')),
    citation        TEXT,
    formula_note    TEXT,                              -- how it's computed from answers
    active          BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE question (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code              TEXT NOT NULL UNIQUE,            -- e.g. 'Q5_smoking'
    version           INT  NOT NULL DEFAULT 1,
    section           TEXT NOT NULL,
    text              TEXT NOT NULL,
    input_type        TEXT NOT NULL,                  -- single_choice | multi_choice | number | year
    options           JSONB,
    feature_key       TEXT REFERENCES feature(key),   -- null for baseline / conditional / multi-feature items
    required          BOOLEAN NOT NULL DEFAULT true,
    active            BOOLEAN NOT NULL DEFAULT true,
    evidence_citation TEXT
);

CREATE TABLE answer (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    profile_id        UUID NOT NULL REFERENCES profile(id) ON DELETE CASCADE,
    question_id       UUID NOT NULL REFERENCES question(id),
    question_version  INT  NOT NULL,                  -- which version was answered
    value             JSONB NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (profile_id, question_id)                  -- one current answer per question
);
CREATE INDEX idx_answer_profile ON answer(profile_id);

CREATE TABLE model_version (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    semver               TEXT NOT NULL UNIQUE,        -- e.g. '2.0.0'
    artifact_uri         TEXT NOT NULL,               -- where the immutable bundle lives
    algorithm            TEXT NOT NULL,               -- 'cox_ph' | 'gbm_survival'
    reference_population  TEXT NOT NULL DEFAULT 'RO_2024',
    data_as_of           DATE,
    is_active            BOOLEAN NOT NULL DEFAULT false,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX one_active_model ON model_version(is_active) WHERE is_active;  -- only one pinned

CREATE TABLE calculation (                            -- APPEND-ONLY (no UPDATE/DELETE)
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id        UUID NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    model_version_id  UUID NOT NULL REFERENCES model_version(id),
    input_hash        TEXT NOT NULL,
    inputs            JSONB NOT NULL,                 -- feature vector + answer snapshot
    estimate_years    NUMERIC NOT NULL,
    interval_low      NUMERIC NOT NULL,
    interval_high     NUMERIC NOT NULL,
    reaches_age       NUMERIC NOT NULL,
    relative_risk     NUMERIC NOT NULL,
    attributions      JSONB NOT NULL,                 -- why[]: {factor, delta_years, evidence, citation}
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_calc_account_time ON calculation(account_id, created_at);  -- progress history

CREATE TABLE scenario (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    base_calculation_id  UUID NOT NULL REFERENCES calculation(id) ON DELETE CASCADE,
    modifications        JSONB NOT NULL,              -- lever changes only
    result               JSONB NOT NULL,              -- years, delta
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE recommendation_rule (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    feature_key        TEXT NOT NULL REFERENCES feature(key),
    condition          JSONB NOT NULL,
    message            TEXT NOT NULL,
    priority           INT  NOT NULL DEFAULT 0,
    evidence_citation  TEXT NOT NULL,                 -- required (evidence traceability)
    active             BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE audit_event (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id    UUID,
    entity      TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    action      TEXT NOT NULL,                        -- create | update | delete | pin_model
    before      JSONB,
    after       JSONB,
    citation    TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
