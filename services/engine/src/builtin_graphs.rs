// Registers the graphs every engine ships with at startup.

use crate::error::EngineError;
use crate::service::Service;

pub const KNOWLEDGE_SEARCH_TOOL_SLUG: &str = "kb_search";

fn definitions() -> [(&'static str, engine_core::Graph); 3] {
    [
        ("simple", engine_core::simple_graph()),
        ("rag", engine_core::rag_graph(KNOWLEDGE_SEARCH_TOOL_SLUG)),
        ("agent", engine_core::agent_graph()),
    ]
}

pub async fn register_all(service: &Service) {
    for (name, definition) in definitions() {
        let definition_json =
            serde_json::to_string(&definition).expect("built-in graphs always serialize");
        match service
            .register_graph_if_changed(name.to_owned(), &definition_json)
            .await
        {
            Ok((_, version)) => tracing::info!(graph = name, version, "built-in graph registered"),
            Err(error) => report(name, &error),
        }
    }
}

fn report(graph: &str, error: &EngineError) {
    tracing::error!(graph, %error, "failed to register built-in graph");
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use sea_orm::EntityTrait;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_boot_leaves_one_row_per_built_in_graph() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());

        register_all(&service).await;
        let after_first_boot = crate::entity::graph::Entity::find()
            .all(&test.db)
            .await
            .expect("query");
        assert_eq!(after_first_boot.len(), definitions().len());

        register_all(&service).await;

        let after_second_boot = crate::entity::graph::Entity::find()
            .all(&test.db)
            .await
            .expect("query");
        assert_eq!(after_second_boot, after_first_boot);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_definition_is_registered_as_the_next_version() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        register_all(&service).await;

        let changed = serde_json::to_string(
            &engine_core::GraphBuilder::new()
                .entry("llm")
                .task("llm", "llm", serde_json::json!({"state_key": "llm"}))
                .end("end")
                .edge("llm", "end", engine_core::Condition::Always)
                .build("simple", 1),
        )
        .expect("serialize");

        let (_, version) = service
            .register_graph_if_changed("simple".to_owned(), &changed)
            .await
            .expect("register");

        assert_eq!(version, 2);
    }
}
