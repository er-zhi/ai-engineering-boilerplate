// The engine.executions table: live executions plus terminal ones inside ENGINE_TERMINAL_RETENTION; every tick claims one row through the partial indexes below.

use sea_orm::entity::prelude::*;

pub const LEASE_OWNER_MAX_LEN: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Status {
    #[sea_orm(num_value = 0)]
    Ready,
    #[sea_orm(num_value = 1)]
    Running,
    #[sea_orm(num_value = 2)]
    Waiting,
    #[sea_orm(num_value = 3)]
    Completed,
    #[sea_orm(num_value = 4)]
    Failed,
    #[sea_orm(num_value = 5)]
    Cancelled,
}

pub const CLAIMABLE_STATUSES: [Status; 3] = [Status::Ready, Status::Running, Status::Waiting];

pub const TERMINAL_STATUSES: [Status; 3] = [Status::Completed, Status::Failed, Status::Cancelled];

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "executions", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub graph_id: String,
    pub graph_version: i32,
    pub user_id: Option<Uuid>,
    pub status: Status,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub wait_kind: Option<Json>,
    #[sea_orm(column_type = "JsonBinary")]
    pub current_nodes: Json,
    pub iteration: i16,
    pub max_iterations: i16,
    pub deadline: Option<DateTimeUtc>,
    #[sea_orm(column_type = "JsonBinary")]
    pub budget: Json,
    #[sea_orm(column_type = "String(StringLen::N(LEASE_OWNER_MAX_LEN))")]
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 2] = [
    "CREATE INDEX IF NOT EXISTS executions_claimable_updated_at_idx \
     ON engine.executions (updated_at) WHERE status IN (0, 1, 2)",
    "CREATE INDEX IF NOT EXISTS executions_terminal_updated_at_idx \
     ON engine.executions (updated_at) WHERE status IN (3, 4, 5)",
];

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveEnum;

    fn status_in_predicate(statuses: &[Status]) -> String {
        let numbers: Vec<String> = statuses
            .iter()
            .map(|status| status.to_value().to_string())
            .collect();
        format!("status IN ({})", numbers.join(", "))
    }

    #[test]
    fn the_index_predicates_list_exactly_the_statuses_they_are_named_for() {
        let [claimable, terminal] = INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;

        assert!(
            claimable.contains(&status_in_predicate(&CLAIMABLE_STATUSES)),
            "{claimable}"
        );
        assert!(
            terminal.contains(&status_in_predicate(&TERMINAL_STATUSES)),
            "{terminal}"
        );
    }
}
