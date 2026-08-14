use anyhow::Result;
use prompt_core::recompute_session;
use rdkafka::{config::ClientConfig, consumer::{CommitMode, Consumer, StreamConsumer}, Message};
use serde_json::Value;
use sqlx::PgPool;
use std::env;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let pool=PgPool::connect(&env::var("DATABASE_URL")?).await?;
    let topics=env::var("KAFKA_TOPICS")?.split(',').map(str::trim).map(str::to_owned).collect::<Vec<_>>();
    let consumer: StreamConsumer=ClientConfig::new().set("group.id",env::var("KAFKA_GROUP_ID").unwrap_or_else(|_|"prompt-analyzer-v1".into())).set("bootstrap.servers",env::var("KAFKA_BOOTSTRAP_SERVERS")?).set("enable.auto.commit","false").set("auto.offset.reset","earliest").create()?;
    consumer.subscribe(&topics.iter().map(String::as_str).collect::<Vec<_>>())?;
    tracing::info!(?topics,"CDC analyzer started");
    loop {
        let message = match consumer.recv().await {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "Kafka receive retry");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        if let Some(payload)=message.payload_view::<str>().transpose()? {
            if let Err(error)=handle_event(&pool,payload).await { tracing::error!(%error,"event analysis failed"); continue; }
        }
        consumer.commit_message(&message,CommitMode::Async)?;
    }
}

async fn handle_event(pool:&PgPool,payload:&str)->Result<()> {
    let event:Value=serde_json::from_str(payload)?;
    let after=&event["payload"]["after"];
    if after.is_null() { return Ok(()); }
    let session_id=if let Some(v)=after.get("session_id").and_then(Value::as_str) { Some(Uuid::parse_str(v)?) }
      else if let Some(run)=after.get("run_id").and_then(Value::as_str) { sqlx::query_scalar("SELECT session_id FROM prompt_runs WHERE id=$1").bind(Uuid::parse_str(run)?).fetch_optional(pool).await? }
      else { None };
    if let Some(id)=session_id { recompute_session(pool,id).await?; tracing::info!(%id,"session metrics refreshed"); }
    Ok(())
}
