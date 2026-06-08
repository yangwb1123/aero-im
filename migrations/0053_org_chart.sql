-- 0053 Org chart / manager hierarchy (reporting lines).
--
-- Models a Lark/Teams-style org chart: each participant may have at most one
-- manager (a self-referential reporting line). One row per participant — the
-- PRIMARY KEY on `participant_id` enforces the "one manager" invariant, and an
-- upsert (ON CONFLICT) re-points an existing line. `set_by` records who last set
-- the line (the participant themselves or an admin). The manager index serves
-- the "direct reports of X" lookup; walking `manager_id` upward yields the full
-- reporting chain (the server caps depth and detects cycles).
--
-- Self-as-own-manager is rejected at the server layer (a 400), not by a DB
-- constraint, to keep the validation message uniform with the rest of the API.
CREATE TABLE IF NOT EXISTS org_reports (
  participant_id uuid PRIMARY KEY,
  manager_id uuid NOT NULL,
  set_by uuid NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS org_reports_manager_idx ON org_reports (manager_id);
