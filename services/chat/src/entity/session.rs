// chat.sessions: one row per user, holding that user's focus pointer.

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
