CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TYPE interaction_type AS ENUM ('NEW_TASK', 'RETRY', 'REFINE');
CREATE TYPE run_status AS ENUM ('DRAFT', 'RUNNING', 'SUCCEEDED', 'FAILED');

CREATE TABLE sessions (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  goal text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE prompt_runs (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  previous_run_id uuid REFERENCES prompt_runs(id),
  sequence integer NOT NULL,
  interaction interaction_type NOT NULL,
  prompt text NOT NULL,
  model text NOT NULL,
  status run_status NOT NULL DEFAULT 'DRAFT',
  response text,
  input_tokens integer NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
  output_tokens integer NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
  cached_tokens integer NOT NULL DEFAULT 0 CHECK (cached_tokens >= 0),
  estimated_input_tokens integer NOT NULL DEFAULT 0 CHECK (estimated_input_tokens >= 0),
  latency_ms bigint,
  cost_usd numeric(14,8) NOT NULL DEFAULT 0,
  clarity_score numeric(5,2) NOT NULL DEFAULT 0,
  specificity_score numeric(5,2) NOT NULL DEFAULT 0,
  structure_score numeric(5,2) NOT NULL DEFAULT 0,
  prompt_score numeric(5,2) NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  completed_at timestamptz,
  UNIQUE (session_id, sequence)
);

CREATE TABLE evaluations (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  run_id uuid NOT NULL UNIQUE REFERENCES prompt_runs(id) ON DELETE CASCADE,
  quality_score numeric(5,2) NOT NULL CHECK (quality_score BETWEEN 0 AND 100),
  accepted boolean NOT NULL,
  reason text,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE session_metrics (
  session_id uuid PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  attempts integer NOT NULL DEFAULT 0,
  accepted boolean NOT NULL DEFAULT false,
  first_try_success boolean NOT NULL DEFAULT false,
  total_tokens bigint NOT NULL DEFAULT 0,
  tokens_to_success bigint,
  retry_waste_tokens bigint NOT NULL DEFAULT 0,
  average_quality numeric(5,2) NOT NULL DEFAULT 0,
  efficiency_score numeric(12,6) NOT NULL DEFAULT 0,
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX prompt_runs_session_idx ON prompt_runs(session_id, sequence);
CREATE INDEX prompt_runs_created_idx ON prompt_runs(created_at DESC);
