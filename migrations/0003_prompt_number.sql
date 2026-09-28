-- A stable, human-friendly number per prompt (1 = oldest), used for ordering and paging the history.
ALTER TABLE prompt_runs ADD COLUMN no bigint;
CREATE SEQUENCE prompt_runs_no_seq OWNED BY prompt_runs.no;
UPDATE prompt_runs r SET no = x.rn FROM (SELECT id, row_number() OVER (ORDER BY created_at, id) AS rn FROM prompt_runs) x WHERE r.id = x.id;
SELECT setval('prompt_runs_no_seq', coalesce((SELECT max(no) FROM prompt_runs), 0) + 1, false);
ALTER TABLE prompt_runs ALTER COLUMN no SET DEFAULT nextval('prompt_runs_no_seq'), ALTER COLUMN no SET NOT NULL;
CREATE UNIQUE INDEX prompt_runs_no_idx ON prompt_runs(no);
