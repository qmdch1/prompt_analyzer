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
    /// The prompt rewritten with the suggestions applied ([...] marks what the user fills in); None when nothing to fix.
    pub improved_prompt: Option<String>,
    pub improved_score: Option<f64>,
}

struct Rules { clarity: f64, specificity: f64, structure: f64, score: f64, has_goal: bool, has_constraints: bool, has_structure: bool }

/// score = clarity 40% + specificity 35% + structure 25%, each from keywords, length and line layout.
fn rules(prompt: &str) -> Rules {
    let words = prompt.split_whitespace().count();
    let has_goal = ["목표", "원하는", "해야", "만들", "분석", "goal"]
        .iter().any(|v| prompt.to_lowercase().contains(v));
    let has_constraints = ["조건", "제약", "반드시", "하지", "format", "형식"]
        .iter().any(|v| prompt.to_lowercase().contains(v));
    let has_structure = prompt.lines().count() >= 3 || prompt.contains("1.") || prompt.contains("-");
    let clarity = (45.0 + words.min(35) as f64 + if has_goal { 20.0 } else { 0.0 }).min(100.0);
    let specificity = (35.0 + if has_constraints { 35.0 } else { 0.0 } + words.min(30) as f64).min(100.0);
    let structure = (40.0 + if has_structure { 45.0 } else { 0.0 } + (prompt.lines().count().min(5) * 3) as f64).min(100.0);
    let score = clarity * 0.4 + specificity * 0.35 + structure * 0.25;
    Rules { clarity, specificity, structure, score, has_goal, has_constraints, has_structure }
}

/// Wraps the prompt in the sections the suggestions ask for, leaving [...] slots for the user.
fn improve(prompt: &str, r: &Rules) -> Option<String> {
    if r.has_goal && r.has_constraints && r.has_structure { return None; }
    let prompt = prompt.trim();
    let mut parts = vec![if r.has_goal { prompt.to_owned() } else { format!("목표: {prompt}\n완료 조건: [무엇이 되면 끝인지 — 예: 테스트 통과, 화면에서 동작 확인]") }];
    if !r.has_constraints { parts.push("제약 조건:\n- 반드시: [지켜야 할 것]\n- 하지 말 것: [건드리면 안 되는 범위]".into()); }
    if !r.has_structure { parts.push("출력 형식:\n- [원하는 결과 형태 — 예: 변경 요약 3줄, 표, 코드만]".into()); }
    Some(parts.join("\n"))
}

fn without_slots(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0;
    for c in text.chars() {
        match c { '[' => depth += 1, ']' if depth > 0 => depth -= 1, _ if depth == 0 => out.push(c), _ => {} }
    }
    out
}

pub fn analyze_prompt(prompt: &str) -> PromptAnalysis {
    let r = rules(prompt);
    let mut suggestions = Vec::new();
    if !r.has_goal { suggestions.push("완료 조건이 드러나는 명확한 목표를 추가하세요.".into()); }
    if !r.has_constraints { suggestions.push("제약 조건과 제외 범위를 명시하세요.".into()); }
    if !r.has_structure { suggestions.push("배경·요구사항·출력 형식으로 나누어 작성하세요.".into()); }
    let improved_prompt = improve(prompt, &r);
    // Score the template without its [...] hints, so blanks don't count as content.
    let improved_score = improved_prompt.as_deref().map(|p| rules(&without_slots(p)).score);
    let estimated = tiktoken_rs::cl100k_base().map(|bpe| bpe.encode_with_special_tokens(prompt).len()).unwrap_or((prompt.chars().count() + 3) / 4);
    PromptAnalysis { estimated_tokens: estimated as i32, clarity_score: r.clarity, specificity_score: r.specificity, structure_score: r.structure, prompt_score: r.score, suggestions, improved_prompt, improved_score }
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
    use super::{analyze_prompt, infer_interaction};

    #[test]
    fn improved_prompt_applies_every_tip() {
        let a = analyze_prompt("커밋하고 푸시해");
        let improved = a.improved_prompt.expect("a bare command has tips to apply");
        assert!(improved.starts_with("목표: 커밋하고 푸시해"));
        let again = analyze_prompt(&improved);
        assert!(again.suggestions.is_empty(), "{:?}", again.suggestions);
        let gain = a.improved_score.unwrap() - a.prompt_score;
        assert!(gain > 20.0 && a.improved_score.unwrap() < 100.0, "blanks must not max out the score: {gain}");
        assert!(analyze_prompt("목표: 결제 모듈 리팩터링\n조건: API 형식 유지\n- 테스트 필수").improved_prompt.is_none());
    }

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
