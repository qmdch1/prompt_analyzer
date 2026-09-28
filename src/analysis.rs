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

/// Guesses how a follow-up prompt relates to the previous attempt in the same agent session.
pub fn infer_interaction(prompt: &str) -> &'static str {
    let p = prompt.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| p.contains(w));
    if has(&["다시", "재시도", "안 돼", "안돼", "안 되", "안되", "에러가", "에러 나", "에러났", "오류가", "오류 나", "오류났", "실패", "여전히", "아직도", "틀렸",
             "again", "retry", "an error", "failed", "not working", "doesn't work", "still"]) {
        "RETRY"
    } else if has(&["말고", "대신", "아니라", "아니고", "수정해", "고쳐", "바꿔", "빠졌", "빠져", "누락", "해야지", "instead", "rather", "missing"]) {
        "REFINE"
    } else {
        "NEW_TASK"
    }
}

#[cfg(test)]
mod tests {
    use super::infer_interaction;

    #[test]
    fn infers_interaction_from_follow_up() {
        for (prompt, expected) in [
            ("이거 구조가 너무 빡센데 단순하고 쉬운구조로 세팅해", "NEW_TASK"),
            ("커밋하고 푸시해", "NEW_TASK"),
            ("에러 처리 로직 추가해줘", "NEW_TASK"),
            ("안돼 다시 해봐", "RETRY"),
            ("빌드하면 에러가 나", "RETRY"),
            ("it still fails", "RETRY"),
            ("PostgreSQL 말고 SQLite로 바꿔", "REFINE"),
            ("1,2,3,4를 자동으로 연결되게 해야지", "REFINE"),
            ("use axum instead", "REFINE"),
        ] {
            assert_eq!(infer_interaction(prompt), expected, "{prompt}");
        }
    }
}
