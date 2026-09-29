//! Automatic recording for agent hooks: one call per prompt, one per finished answer.
use crate::{analysis::{analyze_prompt, clean_prompt, infer_interaction, recompute_session}, err, finish_run, ApiResult, AppState, CompleteRun};
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Deserialize)] pub struct TrackPrompt { source: String, external_id: String, prompt: String, model: Option<String> }
#[derive(Deserialize)] pub struct TrackComplete { source: String, external_id: String, response: String, input_tokens: i32, output_tokens: i32, #[serde(default)] cached_tokens: i32, model: Option<String> }

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) { err(StatusCode::INTERNAL_SERVER_ERROR, e) }

/// Steps 1+2: picks the session (a new task opens a new one) and records the prompt.
pub async fn track_prompt(State(s): State<AppState>, Json(mut p): Json<TrackPrompt>) -> ApiResult<Json<Value>> {
    p.prompt = clean_prompt(&p.prompt);
    if p.prompt.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "prompt is required")); }
    let analysis = analyze_prompt(&p.prompt);
    let mut tx = s.db.begin().await.map_err(internal)?;
    let current: Option<Uuid> = sqlx::query_scalar("SELECT id FROM sessions WHERE source=$1 AND external_id=$2 ORDER BY created_at DESC LIMIT 1 FOR UPDATE")
        .bind(&p.source).bind(&p.external_id).fetch_optional(&mut *tx).await.map_err(internal)?;
    let last: Option<(Uuid, Uuid, i32, String)> = match current {
        Some(id) => sqlx::query_as("SELECT id, session_id, sequence, status::text FROM prompt_runs WHERE session_id=$1 ORDER BY sequence DESC LIMIT 1").bind(id).fetch_optional(&mut *tx).await.map_err(internal)?,
        None => None,
    };
    // Sent while the agent is still answering (or after an interrupted answer): same attempt, one run.
    if let Some((run_id, session_id, _, status)) = &last {
        if status == "DRAFT" || status == "RUNNING" {
            sqlx::query("UPDATE prompt_runs SET prompt = prompt || E'\\n\\n' || $2 WHERE id = $1").bind(run_id).bind(&p.prompt).execute(&mut *tx).await.map_err(internal)?;
            tx.commit().await.map_err(internal)?;
            return Ok(Json(json!({"session_id": session_id, "run_id": run_id, "merged": true, "analysis": analysis})));
        }
    }
    let interaction = if last.is_some() { infer_interaction(&p.prompt) } else { "NEW_TASK" };
    // Step 4 estimate: moving on to a new task means the last attempt worked, asking again means it did not.
    // Explicit ratings are never overwritten.
    if let Some((run_id, _, _, _)) = last {
        sqlx::query("INSERT INTO evaluations(run_id, accepted, reason, auto) VALUES($1, $2, 'auto', true) ON CONFLICT(run_id) DO NOTHING")
            .bind(run_id).bind(interaction == "NEW_TASK").execute(&mut *tx).await.map_err(internal)?;
    }
    let (session_id, previous_run_id, sequence) = match last {
        Some((run_id, session_id, seq, _)) if interaction != "NEW_TASK" => (session_id, Some(run_id), seq + 1),
        _ => {
            let goal: String = p.prompt.chars().take(200).collect();
            let id: Uuid = sqlx::query_scalar("INSERT INTO sessions(goal, source, external_id) VALUES($1,$2,$3) RETURNING id")
                .bind(goal).bind(&p.source).bind(&p.external_id).fetch_one(&mut *tx).await.map_err(internal)?;
            (id, None, 1)
        }
    };
    let model = p.model.unwrap_or_else(|| p.source.clone()); // Claude sends the real model only at Stop
    let run_id: Uuid = sqlx::query_scalar(r#"INSERT INTO prompt_runs(session_id,previous_run_id,sequence,interaction,prompt,model,estimated_input_tokens,clarity_score,specificity_score,structure_score,prompt_score) VALUES($1,$2,$3,$4::interaction_type,$5,$6,$7,$8,$9,$10,$11) RETURNING id"#)
        .bind(session_id).bind(previous_run_id).bind(sequence).bind(interaction).bind(&p.prompt).bind(&model).bind(analysis.estimated_tokens).bind(analysis.clarity_score).bind(analysis.specificity_score).bind(analysis.structure_score).bind(analysis.prompt_score).fetch_one(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    if let Some((_, prev_session, _, _)) = last { recompute_session(&s.db, prev_session).await.map_err(internal)?; }
    recompute_session(&s.db, session_id).await.map_err(internal)?;
    Ok(Json(json!({"session_id": session_id, "run_id": run_id, "interaction": interaction, "analysis": analysis})))
}

/// Step 3: attaches the answer and real token usage to the open run of that agent session.
pub async fn track_complete(State(s): State<AppState>, Json(p): Json<TrackComplete>) -> ApiResult<Json<Value>> {
    let run_id: Uuid = sqlx::query_scalar("SELECT r.id FROM prompt_runs r JOIN sessions s ON s.id=r.session_id WHERE s.source=$1 AND s.external_id=$2 AND r.status='DRAFT' ORDER BY r.created_at DESC LIMIT 1")
        .bind(&p.source).bind(&p.external_id).fetch_optional(&s.db).await.map_err(internal)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no open run for this agent session"))?;
    let done = CompleteRun { response: p.response, input_tokens: p.input_tokens, output_tokens: p.output_tokens, cached_tokens: p.cached_tokens, latency_ms: None };
    Ok(Json(finish_run(&s, run_id, done, p.model).await?))
}

/// Step 4 by voice: rates the latest finished answer of `source`. Returns the rated prompt.
pub async fn rate_last(db: &PgPool, source: &str, quality: f64, accepted: bool, reason: Option<String>) -> anyhow::Result<Option<String>> {
    let mut tx = db.begin().await?;
    // A pure feedback prompt ("좋았어 90점") opened its own single-run session; drop it so it doesn't count as a task.
    sqlx::query(r#"WITH open_run AS (
                     SELECT r.session_id FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
                     WHERE s.source = $1 AND r.status = 'DRAFT' AND r.created_at > now() - interval '10 minutes'
                     ORDER BY r.created_at DESC LIMIT 1)
                   DELETE FROM sessions WHERE id IN (SELECT session_id FROM open_run)
                     AND (SELECT count(*) FROM prompt_runs WHERE session_id = sessions.id) = 1"#)
        .bind(source).execute(&mut *tx).await?;
    let Some((run_id, session_id, prompt)) = sqlx::query_as::<_, (Uuid, Uuid, String)>("SELECT r.id, r.session_id, r.prompt FROM prompt_runs r JOIN sessions s ON s.id=r.session_id WHERE s.source=$1 AND r.status='SUCCEEDED' ORDER BY r.completed_at DESC LIMIT 1")
        .bind(source).fetch_optional(&mut *tx).await? else { return Ok(None) };
    sqlx::query("INSERT INTO evaluations(run_id,quality_score,accepted,reason,auto) VALUES($1,$2,$3,$4,false) ON CONFLICT(run_id) DO UPDATE SET quality_score=excluded.quality_score,accepted=excluded.accepted,reason=excluded.reason,auto=false")
        .bind(run_id).bind(quality).bind(accepted).bind(reason).execute(&mut *tx).await?;
    tx.commit().await?;
    recompute_session(db, session_id).await?;
    Ok(Some(prompt))
}

/// Recent task stats for `source`, plus tips for its latest prompt.
pub async fn stats(db: &PgPool, source: &str, limit: i64) -> anyhow::Result<Value> {
    let mut v: Value = sqlx::query_scalar(r#"SELECT jsonb_build_object(
        'summary', (SELECT jsonb_build_object(
                      'tasks', count(*),
                      'first_try_success_rate_pct', coalesce(round(avg(CASE WHEN m.first_try_success THEN 100 ELSE 0 END)), 0),
                      'total_tokens', coalesce(sum(m.total_tokens), 0),
                      'retry_waste_tokens', coalesce(sum(m.retry_waste_tokens), 0))
                    FROM sessions s JOIN session_metrics m ON m.session_id = s.id WHERE s.source = $1),
        'recent', coalesce((SELECT jsonb_agg(x) FROM (
                      SELECT s.goal, m.attempts, m.accepted, m.total_tokens, m.retry_waste_tokens,
                             (SELECT round(avg(e.quality_score)) FROM evaluations e JOIN prompt_runs r ON r.id = e.run_id WHERE r.session_id = s.id) AS quality,
                             (SELECT round(avg(prompt_score)) FROM prompt_runs WHERE session_id = s.id) AS prompt_score, s.created_at
                      FROM sessions s LEFT JOIN session_metrics m ON m.session_id = s.id
                      WHERE s.source = $1 ORDER BY s.created_at DESC LIMIT $2) x), '[]'::jsonb))"#)
        .bind(source).bind(limit).fetch_one(db).await?;
    let latest: Option<String> = sqlx::query_scalar("SELECT r.prompt FROM prompt_runs r JOIN sessions s ON s.id=r.session_id WHERE s.source=$1 ORDER BY r.created_at DESC LIMIT 1")
        .bind(source).fetch_optional(db).await?;
    if let Some(prompt) = latest {
        let a = analyze_prompt(&prompt);
        v["latest_prompt_tips"] = json!(a.suggestions);
        v["latest_prompt_improved"] = json!(a.improved_prompt);
    }
    Ok(v)
}

/// Is this agent really recording? Called from chat, so the prompt that asked is itself an open run when hooks work.
pub async fn connection(db: &PgPool, source: &str) -> anyhow::Result<String> {
    let (open_ago, answer_ago, total): (Option<f64>, Option<f64>, i64) = sqlx::query_as(r#"
        SELECT (SELECT extract(epoch FROM now() - max(r.created_at))::float8 FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
                WHERE s.source = $1 AND r.status = 'DRAFT' AND r.created_at > now() - interval '30 minutes'),
               (SELECT extract(epoch FROM now() - max(r.completed_at))::float8 FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
                WHERE s.source = $1 AND r.status = 'SUCCEEDED'),
               (SELECT count(*) FROM prompt_runs r JOIN sessions s ON s.id = r.session_id WHERE s.source = $1)"#)
        .bind(source).fetch_one(db).await?;
    let ago = |secs: f64| match secs as i64 { s if s < 60 => format!("{s}초 전"), s if s < 3600 => format!("{}분 전", s / 60), s if s < 86400 => format!("{}시간 전", s / 3600), s => format!("{}일 전", s / 86400) };
    let fix = if source == "codex" { "Codex를 새로 켜서 새 훅을 신뢰(승인)했는지 확인하세요. 터미널에서 `python3 ~/.prompt-analyzer/install.py --check` 로 자세히 볼 수 있습니다." }
              else { "터미널에서 `python3 ~/.prompt-analyzer/install.py --check` 로 원인을 확인하세요." };
    let mut lines = vec![format!("prompt-analyzer 연결 상태 ({source})"), "✓ 서버·MCP 연결됨 (이 도구가 응답함)".to_owned()];
    lines.push(match open_ago {
        Some(s) => format!("✓ 프롬프트 기록 중 (UserPromptSubmit 훅) — 지금 이 프롬프트가 {} 기록됨", ago(s)),
        None => format!("✗ 지금 이 프롬프트가 기록되지 않았습니다. 프롬프트 훅이 동작하지 않는 상태입니다.\n  → {fix}"),
    });
    lines.push(match answer_ago {
        Some(s) => format!("✓ 답변 기록 중 (Stop 훅) — 마지막 답변 {} 기록", ago(s)),
        None => "· 아직 기록된 답변이 없습니다 (답변이 끝나면 토큰과 함께 기록됩니다)".to_owned(),
    });
    lines.push(format!("총 {total}개 기록 · 대시보드 http://localhost:8080"));
    Ok(lines.join("\n"))
}
