// chat.sessions: one row per user — reference data, grows with users, not time, no
// partitioning needed (gate-database "Growth"). One session per user for this scope (see this
// plan's Task 3 design note); focus_topic_id is a pointer, not a topic's own property (spec:
// "Фокус — указатель, не свойство темы"), so it lives here.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions", schema_name = "chat")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub user_id: Uuid,
    pub focus_topic_id: Option<i64>,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
