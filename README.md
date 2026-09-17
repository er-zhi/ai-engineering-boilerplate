# AI Engineering Boilerplate

Production-minded local foundation for AI products: crawl sources, enrich and embed their content, retrieve relevant passages, and route LLM requests without coupling application code to one model.

The project favors simple infrastructure and strict engineering gates: one PostgreSQL instance, isolated schemas, typed Connect/gRPC contracts, bounded workloads, and automated review rules that work well with AI-assisted development.

## What Is Included

- Website crawling with scope controls, SSRF protection, persisted jobs, content extraction, and a discovered-link graph.
- A source-agnostic knowledge base with independent deduplication, LLM enrichment, passage embeddings, and hybrid retrieval using pgvector and PostgreSQL full-text search.
- An LLM router whose callers select a quality tier while configuration selects primary and backup models.
- A single authenticated Gateway serving the browser UI and exposing the public Connect API.
- Review skills under [`.agents/skills/code-review/`](.agents/skills/code-review/) for architecture, correctness, tests, evidence, database design, external facts, and code quality.

This is a working prototype, not merely a specification. It deliberately omits production migrations, distributed queues, rate limiting, and cloud orchestration.

## Main Ideas

- Capabilities are isolated behind versioned contracts; services never query another service's schema.
- PostgreSQL is the database, vector store, and current session store. Extra infrastructure is added only when measurements justify it.
- Expensive AI work is protected by validation, hard limits, deduplication, timeouts, and model fallback.
- Retrieval returns evidence to the calling agent; the knowledge base does not pay for a second answer-generating model.
- Embedding runs locally on Apple Silicon's Neural Engine, keeping search fast and unmetered.
- Code changes are expected to pass deterministic checks before parallel AI review.

The current boundaries and data flow are documented in [Architecture](docs/architecture.md).

## Quick Start

Requirements:

- Apple Silicon Mac (M1 or newer) for the native Core ML embedder.
- Docker with Compose v2.23 or newer.
- An OpenRouter API key.

Configure the stack:

```bash
git clone https://github.com/er-zhi/ai-engineering-boilerplate.git
cd ai-engineering-boilerplate
cp .env.example .env
```

Replace every `change-me` value in `.env`, set `OPENROUTER_API_KEY`, and set `GATEWAY_AUTH_PASSWORD`.

Download or build the uncommitted model files using the [embedder setup instructions](native/embedder-ane/README.md#setup), then start the native embedder in its own terminal:

```bash
cd native/embedder-ane
python3.12 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
uvicorn server:app --host 0.0.0.0 --port 8086
```

Startup can take 30–60 seconds while Core ML compiles the graphs.

Start the rest of the stack from the repository root:

```bash
docker compose up --watch
```

Open <http://localhost:8080> and log in with `GATEWAY_AUTH_PASSWORD`. Without the native embedder, crawling can still fetch pages, but knowledge-base ingestion and search fail.

Detailed host-mode commands, database access, tests, and troubleshooting are in [Development](docs/development.md).

## Documentation

- [Architecture and repository boundaries](docs/architecture.md)
- [Development, testing, and database access](docs/development.md)
- [Crawler](services/crawler/README.md)
- [Knowledge Base](services/knowledge-base/README.md)
- [LLM Router](services/llm-router/README.md)
- [Gateway](services/gateway/README.md)
- [Engine](services/engine/README.md)
- [Tool Service](services/tool/README.md)
- [Frontend](services/frontend/README.md)
- [Native Apple Neural Engine embedder](native/embedder-ane/README.md)
- [Shared contracts and utilities](common/README.md)
- [AI code-review gates](.agents/skills/code-review/SKILL.md)
