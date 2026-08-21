-- Re-queue triage rows foreclosed by the 2026-08-19 fleet LLM outage.
--
-- Background: from 2026-08-19T21:08Z (the daemon-0.0.2 roll) until the tenant
-- virtual keys were re-minted, every LLM call 400'd at the gateway (the VK
-- allowed-models lists predate claude-opus-5). The 0.0.2 passes treated those
-- 400s as row-level permanent failures: Stage-1 stamped 'heuristic-only',
-- Stage-2 stamped the model id, and every affected row kept its ingest
-- heuristic verdict, permanently. 402s (budget exhaustion) did the same to a
-- smaller slice starting 2026-08-17. This script un-stamps those rows so the
-- normal passes re-triage them; it invents no verdicts itself.
--
-- Run per tenant against the live squelch.db (WAL mode tolerates a concurrent
-- writer; the UPDATEs are brief):
--
--   kubectl -n tenants exec deploy/<label> -- sqlite3 \
--     /data/squelch.db < scripts/retriage-foreclosed-2026-08.sql
--
-- (If the image lacks sqlite3, run a debug pod that mounts the tenant PVC.)
-- Rows re-enter the queues at ~10/pass/cycle, so a 400-row backlog drains in
-- under an hour. All rows are younger than the 7-day stale cutoffs as long as
-- this runs before 2026-08-26; after that, re-triage skips them as stale.
--
-- The window starts at the 402 era, not the roll.
--
-- 1) Stage-1 never answered (heuristic seed stands). Clearing stage1_model_used
--    re-queues Stage-1; clearing model_used lets Stage-2 follow its verdict.
--    The reason-prefix guard keeps rows where Stage-2 DID answer (a 402-era
--    Stage-1 fallback whose Stage-2 landed in a later, funded cycle).
UPDATE triage
SET stage1_model_used = NULL, model_used = NULL
WHERE stage1_model_used = 'heuristic-only'
  AND sensitivity != 'sealed'
  AND created_at >= '2026-08-17'
  AND (model_used IS NULL OR reason NOT LIKE 'stage-2%');

-- 2) Stage-1 answered but Stage-2's call failed and the fallback stamped the
--    model id (pre-fix stamp). The stage-1 reason prefix is the fingerprint:
--    a real Stage-2 verdict overwrites reason with 'stage-2 (...'.
UPDATE triage
SET model_used = NULL
WHERE needs_stage2 = 1
  AND model_used = 'claude-opus-5'
  AND reason LIKE 'stage-1%'
  AND sensitivity != 'sealed'
  AND created_at >= '2026-08-17';

-- Post-fix daemons stamp 'stage2-refused' / 'stage2-failed:<kind>' instead of
-- the model id, so this script stays valid: it can never confuse a fallback
-- stamped after the fix with a real opus verdict.
SELECT changes() AS rows_requeued;
