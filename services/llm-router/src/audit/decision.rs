// One row per decision: the statistics worth keeping long after the payloads are gone.

// One row per Decide call, so the table grows with traffic and is bounded only by its monthly partitions,
// which are kept. Nothing reads it outside the tests today: it is queried by hand, by created_at, when
// someone asks what the decisions cost and how often the provider refused them.

use sea_orm::entity::prelude::*;

use crate::audit::request::Outcome;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "decisions", schema_name = "llm_router")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub model_used: String,
    // MAX_QUESTIONS caps a request at 64 questions, which a smallint holds with room to spare.
    pub questions: i16,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub latency_ms: i32,
    pub outcome: Outcome,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const TABLE_STATEMENT: &str = "CREATE TABLE IF NOT EXISTS llm_router.decisions ( \
     id bigserial NOT NULL, \
     model_used varchar(128) NOT NULL, \
     questions smallint NOT NULL, \
     tokens_in integer NOT NULL, \
     tokens_out integer NOT NULL, \
     latency_ms integer NOT NULL, \
     outcome varchar(16) NOT NULL, \
     created_at timestamptz NOT NULL, \
     PRIMARY KEY (created_at, id) \
     ) PARTITION BY RANGE (created_at)";
