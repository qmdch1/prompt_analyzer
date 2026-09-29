//! Import past turns read from agent transcripts by `integrations/install.py --import` on the host.
use crate::{analysis::{analyze_prompt, clean_prompt, infer_interaction, recompute_session}, err, ApiResult, AppState};
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// One answered prompt from a transcript. Timestamps are RFC 3339 (UTC) strings as the agents write them.
#[derive(Deserialize)]
pub struct Turn {
    source: String, external_id: String, prompt: String, #[serde(default)] response: String, model: Option<String>,
    input_tokens: i64, cached_tokens: i64, output_tokens: i64, started_at: String, completed_at: Option<String>,
}
#[derive(Deserialize)] pub struct Batch { turns: Vec<Turn> }

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) { err(StatusCode::INTERNAL_SERVER_ERROR, e) }

/// Adds the turns that aren't recorded yet and groups them into tasks the same way the hooks do.
/// Already recorded means the conversation has a run that starts with the prompt's first paragraph within a minute
/// (hooks and transcripts can append different follow-ups to the same opening prompt)
/// (hooks stamp the server's clock, transcripts the agent's), or that took the prompt as a follow-up
/// sent mid-answer ("…\n\n<prompt>") in the half hour before. The transcript is the source of truth:
/// a recorded run left open (the Stop never arrived) is completed from it, and a finished run takes the
/// transcript's counts when they are larger (a partial copy of a conversation must not shrink them).
pub async fn import(State(s): State<AppState>, Json(b): Json<Batch>) -> ApiResult<Json<Value>> {
    let mut conversations: BTreeMap<(String, String), Vec<Turn>> = BTreeMap::new();
    for mut t in b.turns.into_iter().filter(|t| !t.prompt.trim().is_empty()) {
        t.prompt = clean_prompt(&t.prompt);
        conversations.entry((t.source.clone(), t.external_id.clone())).or_default().push(t);
    }
    let (mut added, mut skipped, mut updated) = (0, 0, 0);
    let mut touched = HashSet::new();
    let mut tx = s.db.begin().await.map_err(internal)?;
    for ((source, external_id), mut turns) in conversations {
        turns.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        let mut last: Option<(Uuid, Uuid, i32)> = None;
        for t in turns {
            let known: Option<(Uuid, Uuid)> = sqlx::query_as(r#"SELECT r.id, r.session_id
                FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
                WHERE s.source = $1 AND s.external_id = $2 AND (
                  (starts_with(r.prompt, left(split_part($3, E'\n\n', 1), 300)) AND abs(extract(epoch FROM r.created_at - $4::timestamptz)) < 60)
                  OR (strpos(r.prompt, E'\n\n' || left($3, 300)) > 0
                      AND r.created_at BETWEEN $4::timestamptz - interval '30 minutes' AND $4::timestamptz + interval '3 minutes'))
                ORDER BY r.created_at LIMIT 1"#)
                .bind(&source).bind(&external_id).bind(&t.prompt).bind(&t.started_at).fetch_optional(&mut *tx).await.map_err(internal)?;
            if let Some((run, session)) = known {
                let fixed = sqlx::query(r#"UPDATE prompt_runs SET
                        input_tokens = $2::int, output_tokens = $3::int, cached_tokens = $4::int,
                        status = 'SUCCEEDED', model = CASE WHEN status = 'DRAFT' THEN coalesce($5, model) ELSE model END,
                        response = coalesce(nullif(response, ''), nullif($6, '')),
                        completed_at = coalesce(completed_at, $7::timestamptz),
                        latency_ms = coalesce(latency_ms, (extract(epoch FROM $7::timestamptz - created_at) * 1000)::bigint)
                    WHERE id = $1 AND (status = 'DRAFT' OR input_tokens + output_tokens < $2::int + $3::int)"#)
                    .bind(run).bind(t.input_tokens).bind(t.output_tokens).bind(t.cached_tokens).bind(&t.model).bind(&t.response).bind(&t.completed_at)
                    .execute(&mut *tx).await.map_err(internal)?;
                if fixed.rows_affected() > 0 { updated += 1; touched.insert(session); }
                skipped += 1;
                last = None;
                continue;
            }
            let interaction = if last.is_some() { infer_interaction(&t.prompt) } else { "NEW_TASK" };
            if let Some((prev, _, _)) = last {
                sqlx::query("INSERT INTO evaluations(run_id, accepted, reason, auto) VALUES($1, $2, 'auto', true) ON CONFLICT(run_id) DO NOTHING")
                    .bind(prev).bind(interaction == "NEW_TASK").execute(&mut *tx).await.map_err(internal)?;
            }
            let (session_id, previous_run_id, sequence) = match last {
                Some((run, session, seq)) if interaction != "NEW_TASK" => (session, Some(run), seq + 1),
                _ => {
                    let goal: String = t.prompt.chars().take(200).collect();
                    let id: Uuid = sqlx::query_scalar("INSERT INTO sessions(goal, source, external_id, created_at) VALUES($1, $2, $3, $4::timestamptz) RETURNING id")
                        .bind(goal).bind(&source).bind(&external_id).bind(&t.started_at).fetch_one(&mut *tx).await.map_err(internal)?;
                    (id, None, 1)
                }
            };
            let a = analyze_prompt(&t.prompt);
            let run: Uuid = sqlx::query_scalar(r#"INSERT INTO prompt_runs(session_id, previous_run_id, sequence, interaction, prompt, model, response, status,
                    input_tokens, output_tokens, cached_tokens, estimated_input_tokens, clarity_score, specificity_score, structure_score, prompt_score,
                    created_at, completed_at, latency_ms)
                VALUES($1, $2, $3, $4::interaction_type, $5, coalesce($6, $7), $8, 'SUCCEEDED', $9::int, $10::int, $11::int, $12, $13, $14, $15, $16,
                    $17::timestamptz, $18::timestamptz, (extract(epoch FROM $18::timestamptz - $17::timestamptz) * 1000)::bigint)
                RETURNING id"#)
                .bind(session_id).bind(previous_run_id).bind(sequence).bind(interaction).bind(&t.prompt).bind(&t.model).bind(&source).bind(&t.response)
                .bind(t.input_tokens).bind(t.output_tokens).bind(t.cached_tokens).bind(a.estimated_tokens)
                .bind(a.clarity_score).bind(a.specificity_score).bind(a.structure_score).bind(a.prompt_score)
                .bind(&t.started_at).bind(&t.completed_at).fetch_one(&mut *tx).await.map_err(internal)?;
            touched.insert(session_id);
            last = Some((run, session_id, sequence));
            added += 1;
        }
    }
    tx.commit().await.map_err(internal)?;
    if added > 0 { renumber(&s).await?; }
    for session in touched { recompute_session(&s.db, session).await.map_err(internal)?; }
    Ok(Json(json!({"added": added, "skipped": skipped, "updated": updated})))
}

/// Past runs arrive after newer ones; renumber so the history (paged by `no`) stays chronological.
async fn renumber(s: &AppState) -> ApiResult<()> {
    let mut tx = s.db.begin().await.map_err(internal)?;
    for sql in [
        "LOCK TABLE prompt_runs IN EXCLUSIVE MODE",
        "UPDATE prompt_runs SET no = -no",
        "UPDATE prompt_runs r SET no = x.rn FROM (SELECT id, row_number() OVER (ORDER BY created_at, id) AS rn FROM prompt_runs) x WHERE r.id = x.id",
        "SELECT setval('prompt_runs_no_seq', (SELECT coalesce(max(no), 0) FROM prompt_runs) + 1, false)",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(())
}
