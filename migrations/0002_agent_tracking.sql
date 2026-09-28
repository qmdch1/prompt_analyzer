-- Sessions recorded automatically from agent hooks (Claude Code, Codex).
ALTER TABLE sessions ADD COLUMN source text, ADD COLUMN external_id text;
CREATE INDEX sessions_external_idx ON sessions(source, external_id, created_at DESC);

-- Auto-estimated evaluations only know whether the attempt worked, not its quality.
ALTER TABLE evaluations ALTER COLUMN quality_score DROP NOT NULL, ADD COLUMN auto boolean NOT NULL DEFAULT false;
