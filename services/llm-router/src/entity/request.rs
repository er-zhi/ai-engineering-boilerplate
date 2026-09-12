// One row per completion: the statistics worth keeping long after the payloads are gone.

use common::proto::llm_router::v1::{FinishReason, QualityTier};
use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(16))")]
pub enum Outcome {
    #[sea_orm(string_value = "answered")]
    Answered,
    #[sea_orm(string_value = "failed")]
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(32))")]
pub enum Tier {
    #[sea_orm(string_value = "unspecified")]
    Unspecified,
    #[sea_orm(string_value = "low")]
    Low,
    #[sea_orm(string_value = "medium")]
    Medium,
    #[sea_orm(string_value = "high")]
    High,
}

impl From<QualityTier> for Tier {
    fn from(tier: QualityTier) -> Self {
        match tier {
            QualityTier::Unspecified => Self::Unspecified,
            QualityTier::Low => Self::Low,
            QualityTier::Medium => Self::Medium,
            QualityTier::High => Self::High,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(32))")]
pub enum Finish {
    #[sea_orm(string_value = "unspecified")]
    Unspecified,
    #[sea_orm(string_value = "stop")]
    Stop,
    #[sea_orm(string_value = "length")]
    Length,
    #[sea_orm(string_value = "content_filter")]
    ContentFilter,
    #[sea_orm(string_value = "tool_calls")]
    ToolCalls,
}

impl From<FinishReason> for Finish {
    fn from(reason: FinishReason) -> Self {
        match reason {
            FinishReason::Unspecified => Self::Unspecified,
            FinishReason::Stop => Self::Stop,
            FinishReason::Length => Self::Length,
            FinishReason::ContentFilter => Self::ContentFilter,
            FinishReason::ToolCalls => Self::ToolCalls,
        }
    }
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "requests", schema_name = "llm_router")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub tier: Tier,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub model_used: String,
    pub used_backup: bool,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub latency_ms: i32,
    pub outcome: Outcome,
    pub finish_reason: Finish,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
