// Declarative tool rows come from a file the operator writes, never from this repository. The code
// here is the capability — read, validate, upsert — and it holds no subject, no endpoint and no
// slug of its own. See gate-architecture, "Capabilities, Not Topics".

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use serde::Deserialize;
use serde_json::Value;

use crate::entity::tool::{ActiveModel, Entity, Model, Risk, Status};
use crate::service::{NewTool, Service};

/// A declarative row only ever issues GETs, so its `output_schema` is unconstrained and is not
/// carried in the file at all — this is the one system-decided shape every row gets.
const DECLARATIVE_OUTPUT_SCHEMA: &str = r#"{"type": "object"}"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeRow {
    pub slug: String,
    pub name: String,
    /// What the model reads to decide whether this tool answers the question in front of it.
    pub description: String,
    pub timeout_seconds: i32,
    pub input_schema: Value,
    pub sources: Value,
}

/// Reads and fully validates a file before anything reaches the database: one bad row refuses the
/// whole file rather than leaving a half-loaded registry.
pub fn read_rows(raw: &str) -> Result<Vec<DeclarativeRow>, String> {
    let rows: Vec<DeclarativeRow> =
        serde_json::from_str(raw).map_err(|error| format!("could not be read as JSON: {error}"))?;
    let mut seen: Vec<&str> = Vec::new();
    for row in &rows {
        if crate::slugs::RESERVED_FOR_SYSTEM_TOOLS.contains(&row.slug.as_str()) {
            return Err(format!("{:?} is a reserved system slug", row.slug));
        }
        if seen.contains(&row.slug.as_str()) {
            return Err(format!("{:?} appears twice", row.slug));
        }
        seen.push(&row.slug);
        crate::tools::declarative::parse_sources(&row.sources, &row.input_schema)
            .map_err(|error| format!("{}: {error}", row.slug))?;
    }
    Ok(rows)
}

/// Upserts every row as an Active system tool. These are operator-supplied definitions, so they skip
/// the LLM review a user-submitted tool goes through — the operator is the reviewer, the same way
/// they are for the tools compiled into this binary.
pub async fn load(service: &Service, db: &DatabaseConnection, path: &str) {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::error!(path, %error, "could not read the declarative tools file");
            return;
        }
    };
    match read_rows(&raw) {
        Ok(rows) => load_rows(service, db, rows).await,
        // Refusing the file loudly and starting without it beats loading half a registry.
        Err(error) => {
            tracing::error!(path, %error, "declarative tools file refused, loading none of it")
        }
    }
}

async fn load_rows(service: &Service, db: &DatabaseConnection, rows: Vec<DeclarativeRow>) {
    for row in rows {
        match upsert(service, db, &row).await {
            Ok(()) => tracing::info!(slug = %row.slug, "loaded declarative tool"),
            Err(error) => {
                tracing::error!(slug = %row.slug, %error, "failed to load declarative tool")
            }
        }
    }
}

async fn upsert(
    service: &Service,
    db: &DatabaseConnection,
    row: &DeclarativeRow,
) -> Result<(), String> {
    let existing = Entity::find()
        .filter(crate::entity::tool::Column::UserId.is_null())
        .filter(crate::entity::tool::Column::Slug.eq(row.slug.as_str()))
        .one(db)
        .await
        .map_err(|error| error.to_string())?;
    match existing {
        Some(stored) => update_declarative_tool(db, stored, row).await,
        None => create_and_activate_declarative_tool(service, db, row).await,
    }
}

async fn create_and_activate_declarative_tool(
    service: &Service,
    db: &DatabaseConnection,
    row: &DeclarativeRow,
) -> Result<(), String> {
    let new_tool = NewTool::checked(
        row.slug.clone(),
        row.name.clone(),
        row.description.clone(),
        row.input_schema.clone(),
        serde_json::from_str(DECLARATIVE_OUTPUT_SCHEMA).expect("a fixed literal always parses"),
        Risk::ReadOnly,
        row.timeout_seconds,
    )
    .map_err(|error| error.to_string())?;
    let tool_id = service
        .create_tool(None, new_tool)
        .await
        .map_err(|error| error.to_string())?;
    let stored = Entity::find_by_id(tool_id)
        .one(db)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "row vanished before activation".to_owned())?;
    update_declarative_tool(db, stored, row).await
}

async fn update_declarative_tool(
    db: &DatabaseConnection,
    stored: Model,
    row: &DeclarativeRow,
) -> Result<(), String> {
    let mut active: ActiveModel = stored.into();
    active.name = Set(row.name.clone());
    active.description = Set(row.description.clone());
    active.input_schema = Set(row.input_schema.clone());
    active.sources = Set(Some(row.sources.clone()));
    active.timeout_seconds = Set(row.timeout_seconds);
    active.risk = Set(Risk::ReadOnly);
    active.status = Set(Status::Active);
    active.update(db).await.map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_ROW: &str = r#"[{
        "slug": "example_reading",
        "name": "Example reading",
        "description": "Returns a reading from several independent providers at once.",
        "timeout_seconds": 8,
        "input_schema": {"type": "object", "required": ["place"],
                         "properties": {"place": {"type": "string"}}},
        "sources": {"fan_out": 2, "take": 1, "sources": [
            {"name": "alpha", "url": "https://alpha.example.com/?q={place}", "pick": "reading.value"},
            {"name": "beta",  "url": "https://beta.example.com/{place}",     "pick": "value"}
        ]}
    }]"#;

    #[test]
    fn a_well_formed_file_is_read_in_full() {
        let rows = read_rows(ONE_ROW).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "example_reading");
        assert_eq!(rows[0].timeout_seconds, 8);
    }

    #[test]
    fn a_row_whose_sources_do_not_parse_is_refused_by_name() {
        let broken = ONE_ROW.replace("{place}", "{elsewhere}");
        let error = read_rows(&broken).expect_err("elsewhere is not an input field");
        assert!(error.contains("example_reading"), "{error}");
        assert!(error.contains("elsewhere"), "{error}");
    }

    #[test]
    fn a_row_claiming_a_reserved_slug_is_refused() {
        let stolen = ONE_ROW.replace("example_reading", crate::slugs::WEB_FETCH);
        let error = read_rows(&stolen).expect_err("a system slug may not be taken");
        assert!(error.contains(crate::slugs::WEB_FETCH), "{error}");
    }

    #[test]
    fn a_duplicate_slug_in_one_file_is_refused() {
        let twice = format!(
            "[{},{}]",
            &ONE_ROW[1..ONE_ROW.len() - 1],
            &ONE_ROW[1..ONE_ROW.len() - 1]
        );
        assert!(read_rows(&twice).is_err());
    }

    #[test]
    fn text_that_is_not_json_names_the_problem_rather_than_panicking() {
        assert!(read_rows("not json").is_err());
    }

    #[test]
    fn an_empty_list_is_fine_and_loads_nothing() {
        assert_eq!(read_rows("[]").expect("read").len(), 0);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod db_tests {
    use super::*;

    const ONE_ROW: &str = r#"[{
        "slug": "example_reading",
        "name": "Example reading",
        "description": "Returns a reading from several independent providers at once.",
        "timeout_seconds": 8,
        "input_schema": {"type": "object", "required": ["place"],
                         "properties": {"place": {"type": "string"}}},
        "sources": {"fan_out": 2, "take": 1, "sources": [
            {"name": "alpha", "url": "https://alpha.example.com/?q={place}", "pick": "reading.value"},
            {"name": "beta",  "url": "https://beta.example.com/{place}",     "pick": "value"}
        ]}
    }]"#;

    async fn service_with(db: DatabaseConnection) -> Service {
        Service::new(
            db,
            "http://127.0.0.1:1",
            "unused-in-these-tests".to_owned(),
            "unused-in-these-tests".to_owned(),
            "http://127.0.0.1:1",
        )
        .expect("service")
    }

    fn write_temp_file(contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "declarative-seed-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    async fn stored_row(db: &DatabaseConnection, slug: &str) -> Model {
        Entity::find()
            .filter(crate::entity::tool::Column::Slug.eq(slug))
            .one(db)
            .await
            .expect("query")
            .expect("row exists")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_loaded_row_is_a_system_row_whose_risk_this_code_decided() {
        let test = crate::test_db::start().await;
        let service = service_with(test.db.clone()).await;
        let path = write_temp_file(ONE_ROW);

        load(&service, &test.db, path.to_str().expect("utf8 path")).await;

        let row = stored_row(&test.db, "example_reading").await;
        assert_eq!(row.user_id, None);
        assert_eq!(row.status, Status::Active);
        assert_eq!(row.risk, Risk::ReadOnly);
        assert!(row.sources.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reloading_updates_the_same_row_rather_than_duplicating_it() {
        let test = crate::test_db::start().await;
        let service = service_with(test.db.clone()).await;
        let path = write_temp_file(ONE_ROW);
        load(&service, &test.db, path.to_str().expect("utf8 path")).await;

        let changed = ONE_ROW.replace("8", "9");
        let path = write_temp_file(&changed);
        load(&service, &test.db, path.to_str().expect("utf8 path")).await;

        let rows = Entity::find()
            .filter(crate::entity::tool::Column::Slug.eq("example_reading"))
            .all(&test.db)
            .await
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timeout_seconds, 9);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_file_is_logged_and_loads_nothing() {
        let test = crate::test_db::start().await;
        let service = service_with(test.db.clone()).await;

        load(&service, &test.db, "/no/such/declarative-tools.json").await;

        let rows = Entity::find().all(&test.db).await.expect("query");
        assert!(rows.is_empty());
    }
}
