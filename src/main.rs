mod analysis;
mod mcp;
mod track;
mod web;

use analysis::{analyze_prompt, recompute_session, PromptAnalysis};
use axum::{extract::{Path, State}, http::StatusCode, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgPool};
use std::{env, sync::Arc, time::Instant};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState { db: PgPool, http: reqwest::Client, cfg: Arc<Config> }

struct Config { api_key: String, model: String, base_url: String, input_rate: f64, output_rate: f64 }

#[derive(Deserialize)] struct CreateSession { goal: String }
#[derive(Deserialize)] struct CreateRun { prompt: String, model: Option<String>, previous_run_id: Option<Uuid>, interaction: Option<String> }
#[derive(Deserialize)] struct CompleteRun { response: String, input_tokens: i32, output_tokens: i32, #[serde(default)] cached_tokens: i32, latency_ms: Option<i64> }
#[derive(Deserialize)] struct EvaluateRun { quality_score: f64, accepted: bool, reason: Option<String> }
#[derive(Serialize, FromRow)] struct Session { id: Uuid, goal: String }
#[derive(Serialize)] struct RunCreated { id: Uuid, session_id: Uuid, sequence: i32, model: String, analysis: PromptAnalysis }

type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;
fn err(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) { (status, Json(json!({"error": e.to_string()}))) }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let db = PgPool::connect(&env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://prompt:prompt@localhost:5432/prompt_analyzer".into())).await?;
    sqlx::migrate!().run(&db).await?;
    let cfg = Config {
        api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
        model: env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5-mini".into()),
        base_url: env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        input_rate: env::var("MODEL_INPUT_USD_PER_MILLION").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
        output_rate: env::var("MODEL_OUTPUT_USD_PER_MILLION").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
    };
    let state = AppState { db, http: reqwest::Client::new(), cfg: Arc::new(cfg) };
    let app = Router::new()
        .route("/", get(web::index))
        .route("/v1/dashboard", get(web::dashboard))
        .route("/v1/prompts", get(web::prompts))
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/sessions/{id}/runs", post(create_run))
        .route("/v1/runs/{id}/complete", post(complete_run))
        .route("/v1/runs/{id}/execute", post(execute_run))
        .route("/v1/runs/{id}/evaluate", post(evaluate_run))
        .route("/v1/sessions/{id}/metrics", get(get_metrics))
        .route("/v1/track/prompt", post(track::track_prompt))
        .route("/v1/track/complete", post(track::track_complete))
        .route("/mcp/{source}", post(mcp::handle))
        .layer(CorsLayer::permissive()).layer(TraceLayer::new_for_http()).with_state(state);
    let addr = format!("0.0.0.0:{}", env::var("PORT").unwrap_or_else(|_| "8080".into()));
    tracing::info!(%addr, "API listening");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn create_session(State(s): State<AppState>, Json(p): Json<CreateSession>) -> ApiResult<(StatusCode, Json<Session>)> {
    if p.goal.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "goal is required")); }
    let row = sqlx::query_as::<_, Session>("INSERT INTO sessions(goal) VALUES($1) RETURNING id, goal").bind(p.goal).fetch_one(&s.db).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn get_session(Path(id): Path<Uuid>, State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let value = sqlx::query_scalar::<_, Value>(r#"SELECT jsonb_build_object('id', s.id, 'goal', s.goal, 'created_at', s.created_at, 'runs', coalesce((SELECT jsonb_agg(to_jsonb(r) ORDER BY r.sequence) FROM prompt_runs r WHERE r.session_id=s.id), '[]'::jsonb)) FROM sessions s WHERE s.id=$1"#).bind(id).fetch_optional(&s.db).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(|| err(StatusCode::NOT_FOUND,"session not found"))?;
    Ok(Json(value))
}

async fn create_run(Path(session_id): Path<Uuid>, State(s): State<AppState>, Json(p): Json<CreateRun>) -> ApiResult<(StatusCode, Json<RunCreated>)> {
    if p.prompt.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "prompt is required")); }
    let analysis = analyze_prompt(&p.prompt);
    let model = p.model.unwrap_or_else(|| s.cfg.model.clone());
    let interaction = p.interaction.unwrap_or_else(|| if p.previous_run_id.is_some() { "REFINE".into() } else { "NEW_TASK".into() });
    if !["NEW_TASK","RETRY","REFINE"].contains(&interaction.as_str()) { return Err(err(StatusCode::BAD_REQUEST,"invalid interaction")); }
    let mut tx = s.db.begin().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE").bind(session_id).fetch_optional(&mut *tx).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(|| err(StatusCode::NOT_FOUND,"session not found"))?;
    let seq: i32 = sqlx::query_scalar("SELECT coalesce(max(sequence),0)+1 FROM prompt_runs WHERE session_id=$1").bind(session_id).fetch_one(&mut *tx).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    let id: Uuid = sqlx::query_scalar(r#"INSERT INTO prompt_runs(session_id,previous_run_id,sequence,interaction,prompt,model,estimated_input_tokens,clarity_score,specificity_score,structure_score,prompt_score) VALUES($1,$2,$3,$4::interaction_type,$5,$6,$7,$8,$9,$10,$11) RETURNING id"#)
        .bind(session_id).bind(p.previous_run_id).bind(seq).bind(&interaction).bind(p.prompt).bind(&model).bind(analysis.estimated_tokens).bind(analysis.clarity_score).bind(analysis.specificity_score).bind(analysis.structure_score).bind(analysis.prompt_score).fetch_one(&mut *tx).await.map_err(|e| err(StatusCode::BAD_REQUEST,e))?;
    tx.commit().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    Ok((StatusCode::CREATED, Json(RunCreated{id,session_id,sequence:seq,model,analysis})))
}

async fn complete_run(Path(id): Path<Uuid>, State(s): State<AppState>, Json(p): Json<CompleteRun>) -> ApiResult<Json<Value>> {
    Ok(Json(finish_run(&s, id, p, None).await?))
}

/// Stores the answer and usage of a run. Latency defaults to the time since the run was created.
async fn finish_run(s: &AppState, id: Uuid, p: CompleteRun, model: Option<String>) -> ApiResult<Value> {
    let cost = p.input_tokens as f64 / 1_000_000.0 * s.cfg.input_rate + p.output_tokens as f64 / 1_000_000.0 * s.cfg.output_rate;
    let session_id: Uuid = sqlx::query_scalar(r#"UPDATE prompt_runs SET response=$2,input_tokens=$3,output_tokens=$4,cached_tokens=$5,latency_ms=coalesce($6,(extract(epoch FROM now()-created_at)*1000)::bigint),cost_usd=$7,model=coalesce($8,model),status='SUCCEEDED',completed_at=now() WHERE id=$1 RETURNING session_id"#).bind(id).bind(p.response).bind(p.input_tokens).bind(p.output_tokens).bind(p.cached_tokens).bind(p.latency_ms).bind(cost).bind(model).fetch_optional(&s.db).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(||err(StatusCode::NOT_FOUND,"run not found"))?;
    recompute_session(&s.db,session_id).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    Ok(json!({"id":id,"status":"SUCCEEDED","cost_usd":cost}))
}

async fn execute_run(Path(id): Path<Uuid>, State(s): State<AppState>) -> ApiResult<Json<Value>> {
    if s.cfg.api_key.is_empty() { return Err(err(StatusCode::SERVICE_UNAVAILABLE,"AI is disabled; set OPENAI_API_KEY")); }
    let (prompt, model): (String,String) = sqlx::query_as("UPDATE prompt_runs SET status='RUNNING' WHERE id=$1 RETURNING prompt,model").bind(id).fetch_optional(&s.db).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(||err(StatusCode::NOT_FOUND,"run not found"))?;
    let started=Instant::now();
    let result: Value = s.http.post(format!("{}/responses",s.cfg.base_url)).bearer_auth(&s.cfg.api_key).json(&json!({"model":model,"input":prompt})).send().await.map_err(|e|err(StatusCode::BAD_GATEWAY,e))?.error_for_status().map_err(|e|err(StatusCode::BAD_GATEWAY,e))?.json().await.map_err(|e|err(StatusCode::BAD_GATEWAY,e))?;
    let response = result
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find_map(|item| item.get("content")?.as_array()?.iter().find_map(|part| part.get("text")?.as_str())))
        .unwrap_or_default()
        .to_owned();
    let usage=&result["usage"];
    finish_run(&s,id,CompleteRun{response:response.clone(),input_tokens:usage["input_tokens"].as_i64().unwrap_or(0) as i32,output_tokens:usage["output_tokens"].as_i64().unwrap_or(0) as i32,cached_tokens:usage["input_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0) as i32,latency_ms:Some(started.elapsed().as_millis() as i64)},None).await?;
    Ok(Json(json!({"id":id,"response":response,"usage":usage})))
}

async fn evaluate_run(Path(id): Path<Uuid>, State(s): State<AppState>, Json(p): Json<EvaluateRun>) -> ApiResult<Json<Value>> {
    if !(0.0..=100.0).contains(&p.quality_score) { return Err(err(StatusCode::BAD_REQUEST,"quality_score must be 0..100")); }
    let session_id: Uuid=sqlx::query_scalar("SELECT session_id FROM prompt_runs WHERE id=$1").bind(id).fetch_optional(&s.db).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(||err(StatusCode::NOT_FOUND,"run not found"))?;
    sqlx::query("INSERT INTO evaluations(run_id,quality_score,accepted,reason) VALUES($1,$2,$3,$4) ON CONFLICT(run_id) DO UPDATE SET quality_score=excluded.quality_score,accepted=excluded.accepted,reason=excluded.reason,auto=false").bind(id).bind(p.quality_score).bind(p.accepted).bind(p.reason).execute(&s.db).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    recompute_session(&s.db,session_id).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    Ok(Json(json!({"run_id":id,"accepted":p.accepted})))
}

async fn get_metrics(Path(id): Path<Uuid>, State(s): State<AppState>) -> ApiResult<Json<Value>> {
    recompute_session(&s.db,id).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?;
    let v=sqlx::query_scalar::<_,Value>("SELECT to_jsonb(m) FROM session_metrics m WHERE session_id=$1").bind(id).fetch_optional(&s.db).await.map_err(|e|err(StatusCode::INTERNAL_SERVER_ERROR,e))?.ok_or_else(||err(StatusCode::NOT_FOUND,"session not found"))?;
    Ok(Json(v))
}
