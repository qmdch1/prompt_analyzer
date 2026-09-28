//! Web dashboard: GET / serves one static page; the page fetches everything below with JS.
use crate::{analysis::analyze_prompt, err, ApiResult, AppState};
use axum::{extract::{Query, State}, http::StatusCode, response::Html, Json};
use serde::Deserialize;
use serde_json::{json, Value};

pub async fn index() -> Html<&'static str> { Html(include_str!("../web/index.html")) }

/// `source`: agent name or "all"; `days`: look-back window (omit for all time); `tz`: IANA zone for daily buckets.
/// `before` / `after`: prompt numbers for paging the history (newest first).
#[derive(Deserialize)]
pub struct Filter { source: Option<String>, days: Option<i32>, tz: Option<String>, before: Option<i64>, after: Option<i64>, limit: Option<i64> }

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) { err(StatusCode::INTERNAL_SERVER_ERROR, e) }

/// Summary tiles and daily tokens for the filtered prompts.
pub async fn dashboard(State(s): State<AppState>, Query(f): Query<Filter>) -> ApiResult<Json<Value>> {
    let source = f.source.filter(|v| v != "all");
    let v: Value = sqlx::query_scalar(r#"
        WITH tz AS (SELECT coalesce((SELECT name FROM pg_timezone_names WHERE name = $3), 'UTC') AS name),
        runs AS (
            SELECT r.*, coalesce(s.source, 'api') AS source FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
            WHERE ($1::text IS NULL OR coalesce(s.source, 'api') = $1)
              AND ($2::int IS NULL OR r.created_at >= now() - make_interval(days => $2))),
        tasks AS (SELECT DISTINCT session_id FROM runs),
        evaluated AS (SELECT DISTINCT r.session_id FROM prompt_runs r JOIN tasks t ON t.session_id = r.session_id JOIN evaluations e ON e.run_id = r.id)
        SELECT jsonb_build_object(
          'sources', coalesce((SELECT jsonb_agg(DISTINCT coalesce(source, 'api')) FROM sessions), '[]'::jsonb),
          'summary', jsonb_build_object(
              'prompts', (SELECT count(*) FROM runs),
              'tasks', (SELECT count(*) FROM tasks),
              'total_tokens', (SELECT coalesce(sum(input_tokens + output_tokens), 0) FROM runs),
              'cached_tokens', (SELECT coalesce(sum(cached_tokens), 0) FROM runs),
              'retry_waste_tokens', (SELECT coalesce(sum(m.retry_waste_tokens), 0) FROM session_metrics m JOIN tasks t ON t.session_id = m.session_id),
              'evaluated_tasks', (SELECT count(*) FROM evaluated),
              'first_try_success_pct', (SELECT round(100.0 * count(*) FILTER (WHERE m.first_try_success) / nullif(count(*), 0))
                                        FROM session_metrics m JOIN evaluated e ON e.session_id = m.session_id),
              'avg_prompt_score', (SELECT round(avg(prompt_score), 1) FROM runs)),
          'daily', coalesce((SELECT jsonb_agg(d ORDER BY d.day) FROM (
              SELECT to_char(r.created_at AT TIME ZONE (SELECT name FROM tz), 'YYYY-MM-DD') AS day, r.source,
                     sum(r.input_tokens + r.output_tokens) AS tokens
              FROM runs r GROUP BY 1, 2) d), '[]'::jsonb))"#)
        .bind(source).bind(f.days).bind(f.tz).fetch_one(&s.db).await.map_err(internal)?;
    Ok(Json(v))
}

/// Prompt history, newest first. Page down with `before=<last no>`, poll for new ones with `after=<first no>`.
pub async fn prompts(State(s): State<AppState>, Query(f): Query<Filter>) -> ApiResult<Json<Value>> {
    let source = f.source.filter(|v| v != "all");
    let limit = f.limit.unwrap_or(30).clamp(1, 100);
    let mut items: Value = sqlx::query_scalar(r#"
        SELECT coalesce(jsonb_agg(x ORDER BY x.no DESC), '[]'::jsonb) FROM (
            SELECT r.no, r.id, coalesce(s.source, 'api') AS source, r.sequence, r.interaction, r.status, r.model,
                   r.prompt, left(r.response, 2000) AS response, r.prompt_score, r.input_tokens, r.cached_tokens, r.output_tokens,
                   r.latency_ms, r.created_at,
                   (SELECT jsonb_build_object('quality_score', e.quality_score, 'accepted', e.accepted, 'auto', e.auto) FROM evaluations e WHERE e.run_id = r.id) AS evaluation
            FROM prompt_runs r JOIN sessions s ON s.id = r.session_id
            WHERE ($1::text IS NULL OR coalesce(s.source, 'api') = $1)
              AND ($2::int IS NULL OR r.created_at >= now() - make_interval(days => $2))
              AND ($3::bigint IS NULL OR r.no < $3)
              AND ($4::bigint IS NULL OR r.no > $4)
            ORDER BY r.no DESC LIMIT $5) x"#)
        .bind(source).bind(f.days).bind(f.before).bind(f.after).bind(limit).fetch_one(&s.db).await.map_err(internal)?;
    let list = items.as_array_mut().map(std::mem::take).unwrap_or_default();
    let next_before = if list.len() as i64 == limit { list.last().and_then(|r| r["no"].as_i64()) } else { None };
    let list: Vec<Value> = list.into_iter().map(|mut r| {
        r["tips"] = json!(analyze_prompt(r["prompt"].as_str().unwrap_or_default()).suggestions);
        r
    }).collect();
    Ok(Json(json!({"items": list, "next_before": next_before})))
}
