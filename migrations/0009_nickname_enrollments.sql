ALTER TABLE nickname_reconcile ADD COLUMN job_id TEXT;
ALTER TABLE nickname_reconcile ADD COLUMN enrollment_id TEXT;
ALTER TABLE nickname_reconcile ADD COLUMN nickname TEXT;

-- Earlier jobs lack a trusted membership and immutable payload. Never
-- adopt them into a current enrollment; a fresh settings request can replace them.
UPDATE nickname_reconcile SET state='dormant', next_attempt_at=0,
    last_error_code='enrollment_required', last_error='fresh nickname request required',
    last_error_at=unixepoch();
