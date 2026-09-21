# Architecture

This document owns cross-service boundaries and data flow. Service-specific behavior belongs in each service README; wire shapes belong in `common/proto/`; database fields belong in the owning service's entities.

## Runtime Flow

```mermaid
flowchart LR
  browser[Browser] -->|HTML and Connect JSON| gateway[Gateway]
  gateway -->|gRPC| crawler[Crawler]
  gateway -->|gRPC Search| kb[Knowledge Base]
  gateway -->|gRPC| chat[Chat]
  crawler -->|gRPC Ingest| kb
  chat -->|gRPC| engine[Engine]
  engine -->|gRPC| tool[Tool]
  engine -->|gRPC Complete/Decide| llm[LLM Router]
  tool -->|gRPC Search| kb
  tool -->|gRPC Complete/Decide| llm
  kb -->|gRPC Complete| llm
  llm -->|HTTPS| openrouter[OpenRouter]
  kb -->|HTTP| embedder[Native ANE embedder]
  gateway --- pg[(PostgreSQL)]
  crawler --- pg
  kb --- pg
  llm --- pg
  chat --- pg
  engine --- pg
  tool --- pg
```

Gateway is the only application service published to the host. It serves Frontend's static output and translates public Connect calls into internal gRPC calls. The browser therefore uses one origin and needs no CORS configuration.

Crawler fetches and denoises pages, stores its crawl record, and submits each successfully stored page to Knowledge Base. Knowledge Base hashes content independently and skips enrichment and embedding when its own stored hash is unchanged. This makes Knowledge Base authoritative for its costs and allows a previously failed hand-off to succeed on a later crawl.

Knowledge Base calls LLM Router for enrichment and the native embedder for document and query vectors. Search combines semantic and lexical passage retrieval and returns evidence; answer generation remains the caller's responsibility.

Chat, Engine and Tool are the agent path, alongside Crawler and Knowledge Base's retrieval path. Gateway forwards chat traffic to Chat, which owns sessions and topics and asks Engine to run each topic as one execution of an agent graph. Engine calls LLM Router directly for a graph's `llm` nodes and Tool for its `tool` nodes; Tool in turn calls Knowledge Base for its two knowledge-base tools, and calls LLM Router both to decide whether a submitted tool definition is approved and, on a refusal, to explain it.

## Boundaries

- Rust services communicate through contracts in `common/proto/`, not through shared tables, files, or internal modules.
- Each persistent service owns one PostgreSQL schema and connects with a role restricted to it. See [schema isolation](../.agents/skills/code-review/gate-database/schema-isolation.md).
- `common` contains wire contracts and code already shared by multiple services, not business logic or repositories.
- Frontend is a one-shot build container. It copies static files to a volume that Gateway mounts read-only; it is not a runtime server or Cargo workspace member.
- `native/embedder-ane` is the only host-native component. Core ML has no Linux-container equivalent, so Knowledge Base reaches it through `host.docker.internal`.
- Provider credentials exist only in LLM Router. Callers request a quality tier for completion, or send typed questions for a decision, and never choose a provider or model slug.
- Chat, Tool and Engine all call `SystemOneService.Decide` for a typed decision — Chat to route a turn, Tool to approve or refuse a submitted tool definition, Engine to pick a tool before an `llm` node spends a generative call — and reach `LlmRouterService.Complete` only where the branch actually needs generated words: Chat's multi-theme split, Tool's refusal explanation, Engine's answer. Both services are served by the LLM Router container, so a caller's outbound surface is that one address whichever of the two it needs; a decision is advisory everywhere it is used, and a decision that fails must never lose the turn.
- Where a typed decision has settled whether a tool is needed, the generative call that follows is not offered that choice again. Engine's `llm` node asks for one of three things: the call alone (tools listed, a plain reply refused, one retry, then a stated failure), the answer alone (no tools listed, so none can be promised), or — only when the decision was unconfident — both, which is the loop with the nudge. A model handed both when the answer is already known replies "I will look that up", and nothing runs after a reply.

## Repository Layout

```text
common/                     shared Rust contracts and utilities
services/crawler/           crawl jobs, pages, and link graph
services/knowledge-base/    enrichment, embeddings, and retrieval
services/llm-router/        tier routing, fallback, structured decisions, and request audit
services/gateway/           authentication, public API, static serving
services/chat/              session and topic orchestration, turn routing
services/engine/            agent graph executions and the tick loop
services/tool/              tool registry and execution, including declarative rows
services/frontend/          browser files copied into Gateway's volume
native/embedder-ane/        host-native Core ML embedding process
.agents/skills/code-review/ automated review gates
```

## Current Constraints

- The stack targets local Apple Silicon development because the embedder uses the Neural Engine.
- Schema synchronization is convenient for development but is not a production migration system.
- Crawl-to-Knowledge-Base delivery has no durable retry queue; a later successful crawl retries the hand-off.
- Any future durable job queue must sit behind a service-owned `Queue` or `JobQueue` trait so domain code depends only on that abstraction. Its first adapter should use the owning service's PostgreSQL schema; SQS, RabbitMQ, or another broker may replace that adapter later without entering business logic.
- One PostgreSQL instance and Docker Compose are intentional until scale measurements require more infrastructure.
- Tables that grow with time rather than with entities (events, audit, usage) are date-partitioned from their first version; hot worker tables hold only live rows. Rationale and reviewer checklist: [gate-database](../.agents/skills/code-review/gate-database/SKILL.md#growth-hot-tables-and-unbounded-tables).
