-- A PR's readiness is GitHub's draft flag alone (the owner, 2026-11-22, D-233: "no let's make it so
-- GitHub is the only source of truth"): the agents' own "ready" flag goes.
UPDATE records SET data = json_remove(data, '$.ready') WHERE kind = 'pr' AND json_extract(data, '$.ready') IS NOT NULL;
