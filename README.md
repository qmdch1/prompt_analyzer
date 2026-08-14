# Prompt Observability

프롬프트와 AI 응답의 품질·토큰·비용·재시도 낭비를 측정하는 Rust 기반 관측/최적화 MVP입니다. PostgreSQL이 원본 저장소이며 Debezium CDC가 변경을 Kafka로 전달하고, analyzer가 세션 지표를 재계산합니다. Redis는 다음 단계의 캐시·락·실시간 집계용으로 구성만 분리해 두었습니다.

## 실행

```bash
cp .env.example .env
docker compose up --build
```

상태 확인:

```bash
curl http://localhost:8080/health
curl http://localhost:8083/connectors/prompt-postgres-cdc/status
```

## 기본 흐름

```bash
# 1. 세션 생성
curl -s http://localhost:8080/v1/sessions -H 'content-type: application/json' \
  -d '{"goal":"PostgreSQL CDC 구축"}'

# 2. 프롬프트 추가 (응답에 즉시 휴리스틱 분석과 예상 토큰 포함)
curl -s http://localhost:8080/v1/sessions/SESSION_ID/runs -H 'content-type: application/json' \
  -d '{"prompt":"Debezium 기반 CDC를 구성해줘. Docker Compose와 검증 절차를 포함해.","interaction":"NEW_TASK"}'

# 3-A. 외부에서 받은 AI 결과/사용량 기록
curl -s http://localhost:8080/v1/runs/RUN_ID/complete -H 'content-type: application/json' \
  -d '{"response":"...","input_tokens":120,"output_tokens":450,"latency_ms":1800}'

# 3-B. 또는 OpenAI 연동 후 직접 실행
curl -s -X POST http://localhost:8080/v1/runs/RUN_ID/execute

# 4. 평가 (다음 요청은 previous_run_id와 RETRY/REFINE으로 연결)
curl -s http://localhost:8080/v1/runs/RUN_ID/evaluate -H 'content-type: application/json' \
  -d '{"quality_score":82,"accepted":true,"reason":"요구사항 충족"}'

# 5. 효율 지표
curl -s http://localhost:8080/v1/sessions/SESSION_ID/metrics
```

## 지표 정의

- `total_tokens`: 세션 내 실제 입력+출력 토큰
- `tokens_to_success`: 최초 승인 응답까지 누적 토큰
- `retry_waste_tokens`: 최초 승인 전 실패 시도에 소비된 토큰
- `first_try_success`: 첫 시도 승인 여부
- `efficiency_score`: 평균 품질 점수 × 1,000 / 총 토큰
- 프롬프트 생성 시 `clarity`, `specificity`, `structure`, `prompt_score`와 개선안을 즉시 반환

토큰 단가는 모델·시점별로 달라지므로 코드에 고정하지 않고 `.env`의 백만 토큰당 USD 값으로 관리합니다. API 키가 없어도 기록·분석 기능은 모두 동작합니다.

## 구조

```text
apps/api        세션/프롬프트/응답/평가 REST API + 선택적 OpenAI 어댑터
apps/analyzer   Debezium CDC Kafka consumer
crates/core     토큰 추정·프롬프트 분석·세션 지표 계산
migrations      PostgreSQL 스키마
infra/debezium  Connector 자동 등록
```

운영 전에는 인증/테넌트, 비밀 관리, DLQ와 재처리, 모델별 가격 이력, Redis 캐시 및 대시보드를 추가하는 것을 권장합니다.
