use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAnalysis {
    pub estimated_tokens: i32,
    pub clarity_score: f64,
    pub specificity_score: f64,
    pub structure_score: f64,
    pub prompt_score: f64,
    pub suggestions: Vec<String>,
}
pub fn analyze_prompt(prompt: &str) -> PromptAnalysis {
    let words = prompt.split_whitespace().count();
    let has_goal = ["목표", "원하는", "해야", "만들", "분석", "goal"]
        .iter().any(|v| prompt.to_lowercase().contains(v));
    let has_constraints = ["조건", "제약", "반드시", "하지", "format", "형식"]
        .iter().any(|v| prompt.to_lowercase().contains(v));
    let has_structure = prompt.lines().count() >= 3 || prompt.contains("1.") || prompt.contains("-");
    let clarity = (45.0 + words.min(35) as f64 + if has_goal { 20.0 } else { 0.0 }).min(100.0);
    let specificity = (35.0 + if has_constraints { 35.0 } else { 0.0 } + words.min(30) as f64).min(100.0);
    let structure = (40.0 + if has_structure { 45.0 } else { 0.0 } + (prompt.lines().count().min(5) * 3) as f64).min(100.0);
    let mut suggestions = Vec::new();
    if !has_goal { suggestions.push("완료 조건이 드러나는 명확한 목표를 추가하세요.".into()); }
    if !has_constraints { suggestions.push("제약 조건과 제외 범위를 명시하세요.".into()); }
    if !has_structure { suggestions.push("배경·요구사항·출력 형식으로 나누어 작성하세요.".into()); }
    let estimated = tiktoken_rs::cl100k_base().map(|bpe| bpe.encode_with_special_tokens(prompt).len()).unwrap_or((prompt.chars().count() + 3) / 4);
    let score = clarity * 0.4 + specificity * 0.35 + structure * 0.25;
    PromptAnalysis { estimated_tokens: estimated as i32, clarity_score: clarity, specificity_score: specificity, structure_score: structure, prompt_score: score, suggestions }
}

pub async fn recompute_session(pool: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO session_metrics
           (session_id, attempts, accepted, first_try_success, total_tokens, tokens_to_success,
            retry_waste_tokens, average_quality, efficiency_score, updated_at)
           SELECT s.id,
                  count(r.id)::int,
                  coalesce(bool_or(e.accepted), false),
                  coalesce(bool_or(e.accepted AND r.sequence = 1), false),
                  coalesce(sum(r.input_tokens + r.output_tokens), 0)::bigint,
                  sum(r.input_tokens + r.output_tokens) FILTER (WHERE r.sequence <= accepted_seq)::bigint,
                  coalesce(sum(r.input_tokens + r.output_tokens) FILTER (WHERE accepted_seq IS NOT NULL AND r.sequence < accepted_seq), 0)::bigint,
                  coalesce(avg(e.quality_score), 0),
                  CASE WHEN coalesce(sum(r.input_tokens + r.output_tokens), 0) = 0 THEN 0
                       ELSE coalesce(avg(e.quality_score), 0) * 1000 / sum(r.input_tokens + r.output_tokens) END,
                  now()
           FROM sessions s
           LEFT JOIN (SELECT pr.*, min(pr.sequence) FILTER (WHERE ev.accepted) OVER (PARTITION BY pr.session_id) accepted_seq
                      FROM prompt_runs pr LEFT JOIN evaluations ev ON ev.run_id = pr.id) r ON r.session_id = s.id
           LEFT JOIN evaluations e ON e.run_id = r.id
           WHERE s.id = $1
           GROUP BY s.id
           ON CONFLICT (session_id) DO UPDATE SET
             attempts=excluded.attempts, accepted=excluded.accepted, first_try_success=excluded.first_try_success,
             total_tokens=excluded.total_tokens, tokens_to_success=excluded.tokens_to_success,
             retry_waste_tokens=excluded.retry_waste_tokens, average_quality=excluded.average_quality,
             efficiency_score=excluded.efficiency_score, updated_at=excluded.updated_at"#,
    ).bind(session_id).execute(pool).await?;
    Ok(())
}
