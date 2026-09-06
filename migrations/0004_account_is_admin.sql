-- Admin flag on accounts, gating the admin/* surface. Admins are promoted out-of-band (ops SQL or a
-- bootstrap step); no self-service path. Default false, so existing accounts stay non-admin.

ALTER TABLE account ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT false;
