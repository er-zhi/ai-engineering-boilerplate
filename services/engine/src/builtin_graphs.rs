// Registers the graphs every engine ships with at startup.

use crate::service::Service;

const RETRIEVAL_TOOL_SLUG_VAR: &str = "RAG_RETRIEVAL_TOOL_SLUG";

fn retrieval_tool_slug() -> Option<String> {
    let slug = std::env::var(RETRIEVAL_TOOL_SLUG_VAR).ok()?;
    let slug = slug.trim().to_owned();
    (!slug.is_empty()).then_some(slug)
}

fn definitions() -> Vec<(&'static str, engine_core::Graph)> {
    let mut graphs = vec![
        ("simple", engine_core::simple_graph()),
        ("agent", engine_core::agent_graph()),
    ];
    match retrieval_tool_slug() {
        Some(slug) => graphs.push(("rag", engine_core::rag_graph(&slug))),
        None => tracing::info!(
            var = RETRIEVAL_TOOL_SLUG_VAR,
            "no retrieval tool named, so the rag graph is not registered"
        ),
    }
    graphs
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
            Err(error) => {
                tracing::error!(graph = name, %error, "failed to register built-in graph")
            }
        }
    }
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

    #[test]
    fn a_retrieval_graph_is_registered_only_when_a_tool_is_named_for_it() {
        // Which tool retrieves is the Tool service's to own and an operator's to withdraw, so the
        // slug is theirs to name. Declaring it here would register a node pointing at a slug that
        // `built-in-tools.json` can take out of the catalog.
        assert!(
            !definitions().iter().any(|(name, _)| *name == "rag"),
            "with no tool named, the rag graph must not be registered against a guessed slug"
        );
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
