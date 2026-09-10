-- Real measured settlements replace the seven invented Romanian rows.
--
-- `seeds/locations.json` described itself as ILLUSTRATIVE and said real layers had to replace it before
-- the "Where Should I Live?" surface shipped. It shipped anyway, and because `seed_locations` is
-- upsert-only, "Bucharest, pm25 = 19.0" survives in any database that has ever booted even after the
-- seed file stops mentioning it. So the deletion has to be a migration, not an absence.

-- Provenance, per row. A measured value with no year and no source is indistinguishable from an
-- invented one the next time somebody asks where it came from.
ALTER TABLE location ADD COLUMN IF NOT EXISTS iso3        TEXT;
ALTER TABLE location ADD COLUMN IF NOT EXISTS lat         DOUBLE PRECISION;
ALTER TABLE location ADD COLUMN IF NOT EXISTS lon         DOUBLE PRECISION;
ALTER TABLE location ADD COLUMN IF NOT EXISTS population  BIGINT;
ALTER TABLE location ADD COLUMN IF NOT EXISTS pm25_year   INT;
ALTER TABLE location ADD COLUMN IF NOT EXISTS pm25_stations INT;
ALTER TABLE location ADD COLUMN IF NOT EXISTS ndvi_year   INT;
-- 'city' = measured in this settlement. 'country' = this country's figure, shown here because this
-- settlement has none. NULL = no greenness at all. The screen must print the distinction, so the
-- database has to carry it.
ALTER TABLE location ADD COLUMN IF NOT EXISTS ndvi_basis  TEXT;
ALTER TABLE location ADD COLUMN IF NOT EXISTS source      TEXT;

ALTER TABLE location DROP CONSTRAINT IF EXISTS location_ndvi_basis_ck;
ALTER TABLE location ADD CONSTRAINT location_ndvi_basis_ck CHECK (
    (ndvi IS NULL AND ndvi_basis IS NULL) OR
    (ndvi IS NOT NULL AND ndvi_basis IN ('city', 'country'))
);

-- A profile whose home is one of the fakes. Its stored exposure was fiction, so its estimate changes
-- whatever we do; NULL is the honest option. Re-pointing it at the nearest real row would decide that
-- this reader lives in Bucureşti and silently re-price them for it.
UPDATE profile SET home_location_id = NULL
WHERE home_location_id IN (
    SELECT id FROM location WHERE source IS NULL AND as_of = DATE '2024-01-01'
);

-- The fakes themselves. Identified by what they have rather than by name: every real row inserted from
-- here on carries a `source`, so "no source" is exactly the set of rows that predate this migration.
DELETE FROM location WHERE source IS NULL;

-- 3,522 settlements across 85 countries, looked up by country and by name.
CREATE INDEX IF NOT EXISTS location_country_idx ON location (country);
CREATE INDEX IF NOT EXISTS location_iso3_idx ON location (iso3);
