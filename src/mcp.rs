//! Minimal MCP server (Streamable HTTP, JSON responses) at /mcp/{source}, e.g. /mcp/claude or /mcp/codex.
use crate::{track, AppState};
use axum::{extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::{json, Value};

pub async fn handle(Path(source): Path<String>, State(s): State<AppState>, Json(req): Json<Value>) -> Response {
    // Notifications (no id) need no answer.
    let Some(id) = req.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let params = &req["params"];
    let result = match req["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "prompt-analyzer", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call_tool(&s, &source, params).await),
        method => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    let body = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    };
    Json(body).into_response()
}

fn tools() -> Value {
    json!([
        {
            "name": "rate_last_answer",
            "description": "사용자가 직전 답변을 평가하면 호출합니다 (예: '좋았어 90점', '별로야', '이건 틀렸어', 'that worked'). Records the user's rating of your previous answer in prompt-analyzer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "quality_score": {"type": "number", "minimum": 0, "maximum": 100, "description": "0-100. 점수를 말하지 않았으면 말투로 추정 (만족 80-95, 보통 60, 불만 20-40)"},
                    "accepted": {"type": "boolean", "description": "결과를 그대로 받아들였으면 true, 다시 해야 하면 false"},
                    "reason": {"type": "string", "description": "사용자가 말한 이유 (선택)"}
                },
                "required": ["quality_score", "accepted"]
            }
        },
        {
            "name": "prompt_stats",
            "description": "최근 작업들의 프롬프트 점수, 토큰 사용량, 재시도 낭비, 성공 여부와 최근 프롬프트 개선 팁을 보여줍니다. Shows recent prompt efficiency stats.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "최근 작업 몇 개를 볼지 (기본 10)"}}
            }
        }
    ])
}

async fn call_tool(s: &AppState, source: &str, params: &Value) -> Value {
    let args = &params["arguments"];
    let outcome = match params["name"].as_str().unwrap_or_default() {
        "rate_last_answer" => match (args["quality_score"].as_f64(), args["accepted"].as_bool()) {
            (Some(q), Some(accepted)) if (0.0..=100.0).contains(&q) => {
                let reason = args["reason"].as_str().map(str::to_owned);
                track::rate_last(&s.db, source, q, accepted, reason).await.map(|rated| match rated {
                    Some(prompt) => format!("기록했습니다: {q}점, {} — \"{}\"", if accepted { "성공" } else { "실패" }, prompt.chars().take(60).collect::<String>()),
                    None => "아직 평가할 답변이 없습니다.".into(),
                })
            }
            _ => Err(anyhow::anyhow!("quality_score(0-100)와 accepted가 필요합니다")),
        },
        "prompt_stats" => track::stats(&s.db, source, args["limit"].as_i64().unwrap_or(10).clamp(1, 50)).await.map(|v| v.to_string()),
        name => Err(anyhow::anyhow!("unknown tool: {name}")),
    };
    match outcome {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}),
    }
}
