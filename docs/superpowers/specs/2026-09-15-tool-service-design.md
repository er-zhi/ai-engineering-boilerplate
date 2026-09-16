# Tool Service

Реализует раздел 3 «Tool Service» из `.temp/plans/2026-09-12.md` и раздел «2. Tool Service» из
`.temp/plans/todo.md`. Второй сервис после Engine (`docs/superpowers/specs/2026-09-15-engine-design.md`),
единственная цель которого — дать реальную реализацию `kind = "tool"`, которую
`services/engine/src/dispatch.rs::Dispatcher` сегодня явно отклоняет («no TaskExecutor wired for
kind "tool" yet»), и сделать `agent_graph()`/`rag_graph()` (уже встроены в Engine) рабочими
end-to-end, а не только структурно корректными.

## Что это и чем не является

Tool Service — реестр тулов (`tools`) плюс их исполнитель (`Execute`), с детерминированной
(не LLM) policy по риску. Он не хранит и не выдаёт долгоживущие креды третьих сторон — Integrations
Service, через который это должно идти по итоговой архитектуре (`Engine → Tool Service →
Integrations → внешний API`), сегодня не построен. Единственный секрет, который держит сам Tool
Service, — ключ системного `web_search` (`BRAVE_SEARCH_API_KEY`), явно временно, до Integrations.

Tool Service не решает, вызывать ли тул повторно при retry Engine — это ответственность
Engine/адаптера-исполнителя (`idempotency_key` передаётся и логируется, как и в `llm`-адаптере,
но не проверяется: ни один тул этого спека не имеет разрушительного побочного эффекта, который
нужно было бы гасить).

## Три развилки, разрешённые при брейнсторминге

1. **LLM не видит список тулов.** `build_prompt` (`services/engine/src/executors/llm.rs`) сегодня
   не перечисляет доступные тулы. `Dispatcher`, перед вызовом `llm`-исполнителя для узла с
   `config.tool_calling == true`, сам дёргает `Tool Service.ListTools` и кладёт каталог в копию
   `config` (`config["available_tools"] = [...]`) — существующий порт `TaskExecutor::execute(kind,
   config, state, key)` для этого не меняется, потому что `config` уже параметр, который
   `Dispatcher` волен модифицировать перед передачей дальше. `build_prompt` добавляет каталог в
   system prompt, только когда `tool_calling` и `available_tools` оба заданы.
2. **`Execute` на Write/Destructive тул сегодня не выполняется.** Approval-flow (`Wait::Approval`)
   в Engine сознательно не достроен (как и wiring `Subgraph`, см. спек Engine, «Вне скоупа»).
   `Execute` возвращает `status = "requires_approval"` без выполнения; `ToolTaskExecutor` в Engine
   превращает это в понятную ошибку, а не зависание или тихий пропуск проверки.
3. **`user_id` при вызове тула из графа не передаётся сегодня.** Порт `TaskExecutor::execute` в
   `engine-core` не несёт владельца. Единственные тулы, которые сегодня вызываются из графа
   (`agent_graph`/`rag_graph`), — системные (`user_id = NULL`), их `Execute` находит без контекста
   пользователя. Вызов пользовательского тула по имени из графа — будущая задача (когда Chat даст
   на него сослаться); тогда порт получит одно новое поле. `engine-core` сегодня не трогаем.

## Модель

### `tools`

```rust
struct Tool {
    id: ToolId,                    // i64, суррогатный PK
    user_id: Option<UserId>,       // None = системный
    slug: String,                  // уникален в рамках владельца — см. «Схема БД»
    name: String,
    description: String,
    input_schema: serde_json::Value,   // JSON Schema, как строка/jsonb — не типизировано глубже
    output_schema: serde_json::Value,
    connection_id: Option<i64>,    // не используется сегодня — задел под Integrations
    risk: Risk,
    timeout: Duration,
    status: ToolStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

enum Risk { ReadOnly, Write, Destructive }
enum ToolStatus { Draft, Validated, Active }
```

### Policy — чистая функция, не LLM

```rust
enum PolicyDecision { Execute, RequiresApproval }
fn check_policy(risk: Risk) -> PolicyDecision {
    match risk { Risk::ReadOnly => PolicyDecision::Execute, Risk::Write | Risk::Destructive => PolicyDecision::RequiresApproval }
}
```

Три случая, три юнит-теста, без сети и без БД — тот же принцип, что `evaluate_condition` в
`engine-core`: детерминированная логика живёт в чистой функции, не разбросана по хендлерам.

## Системные тулы

Регистрируются в БД при старте сервиса (как `simple`/`rag`/`agent` у Engine — не через RPC),
все `Risk::ReadOnly`, `status = Active`, `user_id = None`:

| slug | Реализация |
|---|---|
| `web_search` | `SearchProvider` trait + `BraveSearchProvider` (reqwest → `api.search.brave.com`, заголовок `X-Subscription-Token`, ключ — `BRAVE_SEARCH_API_KEY`). Ответ: `[{title, url, snippet}]`. |
| `web_fetch` | reqwest GET → `common::extract::extract(url, &html)` (см. «Переезд `extract`» ниже) → `{title, text}`. Байтовый предел ответа — тот же порядок, что `crawler::links::MAX_PARSEABLE_HTML_BYTES`, чтобы не тащить в память нефильтрованный HTML. |
| `kb_search` | Тонкая обёртка над `knowledge_base.v1.KnowledgeBaseService/Search` (уже существует) — тот же клиентский паттерн, что `services/knowledge-base/src/llm_router_client.rs` использует для вызова llm-router. |
| `kb_read_document` | Обёртка над `knowledge_base.v1.KnowledgeBaseService/ReadDocument`. |

### Переезд `extract`

`services/crawler/src/extract.rs` → `common/src/extract.rs` (модуль `common::extract`) — второй
реальный потребитель (`web_fetch`) появляется сегодня, что и было условием переезда в `common` во
всех прежних решениях этой сессии. `crawler` переключает свой единственный вызов на
`common::extract::extract`, сам `extract.rs` в crawler удаляется. Публичный тип `Extracted`
(`title`, `main_text`, `content_hash`) не меняется.

## Пользовательские тулы

`CreateTool` (статус `Draft`) → `ValidateTool` (один вызов llm-router, medium tier: проверяет, что
`input_schema`/`output_schema` — валидный JSON Schema, `description` понятно описывает, что делает
тул и его последствия, `timeout` разумен; ответ `{approved: bool, feedback: string}`, статус
становится `Validated` или остаётся `Draft` с `feedback`) → `ActivateTool` (только из `Validated`,
делает `Active`). Фронтенд, который дёргает эти три RPC кнопками «Валидировать»/«Активировать», —
задача Chat Service; сегодня строится только сама RPC-поверхность.

## gRPC-поверхность (Connect RPC, `common/proto/tools/v1`)

| Метод | Назначение |
|---|---|
| `CreateTool(slug, name, description, input_schema_json, output_schema_json, risk, timeout_seconds) -> {tool_id}` | `status = Draft`; `user_id` — из `Principal`, никогда из тела |
| `ValidateTool(tool_id) -> {approved, feedback}` | требует, чтобы `tool.user_id` совпадал с `Principal.user_id` — системные тулы (`user_id = None`) уже `Active` с момента посева и никогда не проходят через `Draft`, так что этот вызов на них просто не находит владельца и отклоняется |
| `ActivateTool(tool_id) -> {}` | требует `status = Validated` |
| `ListTools() -> {tools: [...]}` | системные + тулы вызывающего (`Principal`) |
| `Execute(slug, input_json, idempotency_key) -> {status, output_json, error_message}` | `status`: `"ok" \| "requires_approval" \| "error"`; поиск — сперва тул вызывающего с этим `slug`, иначе системный |

## `ToolTaskExecutor` — второй `TaskExecutor` в Engine

```rust
pub struct ToolTaskExecutor { client: ToolServiceClient<HttpClient> }
impl TaskExecutor for ToolTaskExecutor {
    async fn execute(&self, kind, config, state, idempotency_key) -> Result<Value, TaskError> {
        // kind == "tool"; вызов читается из состояния, не из config — конвенция agent_graph()
        let call = state.pointer("/llm/tool_call")...;   // {name, args}
        let response = self.client.execute(ExecuteRequest {
            slug: call["name"], input_json: call["args"].to_string(), idempotency_key,
        }).await?;
        match response.status.as_str() {
            "ok" => Ok(serde_json::from_str(&response.output_json)?),
            "requires_approval" => Err(TaskError::Failed("tool requires approval — not yet wired in Engine".into())),
            _ => Err(TaskError::Failed(response.error_message)),
        }
    }
}
```

`services/engine/src/dispatch.rs::Dispatcher` получает поле `tool: ToolTaskExecutor` и ветку
`"tool" => self.tool.execute(...)` в своём `match kind`; при `kind == "llm"` и
`config.tool_calling == true` сначала вызывает `ToolTaskExecutor::list_tools()` (лёгкий метод,
использующий тот же клиент) и передаёт результат в изменённый `config` для `LlmTaskExecutor` —
см. развилку 1 выше.

## Схема БД (`tool`)

```sql
CREATE TABLE tool.tools (
  id              bigserial PRIMARY KEY,
  user_id         uuid NULL,
  slug            varchar(64) NOT NULL,
  name            varchar(128) NOT NULL,
  description     text NOT NULL,
  input_schema    jsonb NOT NULL,
  output_schema   jsonb NOT NULL,
  connection_id   bigint NULL,
  risk            smallint NOT NULL,
  timeout_seconds integer NOT NULL,
  status          smallint NOT NULL,
  created_at      timestamptz NOT NULL,
  updated_at      timestamptz NOT NULL
);
```

Уникальность `slug` в рамках владельца **не выражается** стандартным SeaORM `unique_key` на
`(user_id, slug)`: Postgres считает `NULL <> NULL`, значит обычный `UNIQUE(user_id, slug)`
пропустит два разных системных тула с одинаковым slug — ровно дыра, которую нашли на
брейнсторминге. Индекс — ручной, тем же санкционированным механизмом, что уже использован для
частичного индекса `executions` и партиций `execution_events` (`gate-database`, исключение
«вендор-специфичный объект схемы»):

```sql
CREATE UNIQUE INDEX IF NOT EXISTS tools_owner_slug_idx
  ON tool.tools (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid), slug);
```

JSON-колонки — `jsonb` (`column_type = "JsonBinary"`), без исключений, по правилу
`gate-database` → «Column Types». Таблица растёт с числом тулов, не со временем — обычная
reference-таблица (`gate-database` → «Growth»), партиционирование не нужно.

## Тесты

- `policy.rs`: три случая `check_policy`, без сети и БД.
- `providers/brave.rs`, `web_fetch.rs`, `kb_client.rs`: fake-сервер в процессе (тот же паттерн,
  что `services/knowledge-base/src/llm_router_client.rs`'s `FakeLlmRouter` и Engine's Task 14
  fake-llm-router-тест) — без реального сетевого вызова и без `BRAVE_SEARCH_API_KEY`.
- `service.rs` (testcontainers, реальный Postgres): `CreateTool` → `Draft`; `ValidateTool` через
  fake llm-router → `Validated`/`Draft` с feedback; `ActivateTool` из не-`Validated` — ошибка;
  `ListTools` не показывает чужие пользовательские тулы; `Execute` на `ReadOnly` выполняется, на
  `Write`/`Destructive` — `requires_approval`; два системных тула с одинаковым `slug` —
  нарушение уникальности (доказывает, что ручной индекс действительно стоит).
- `services/engine`: `ToolTaskExecutor` — юнит-тест с fake `ToolService`-сервером; `Dispatcher`
  подставляет каталог тулов в `config` перед вызовом `llm`, только когда `tool_calling = true`.

## Вне скоупа этого спека

- Integrations Service, `connection_id` реально используется только когда он появится.
- Approval-flow для `Write`/`Destructive` (`Wait::Approval` в Engine) — остаётся заглушкой,
  Tool Service лишь корректно сигнализирует `requires_approval`.
- Вызов пользовательского тула по имени из графа (`user_id` через порт `TaskExecutor`).
- Фронтенд «Валидировать»/«Активировать» — Chat Service.
- Реальная проверка `web_search` живым Brave-ключом — нужен `BRAVE_SEARCH_API_KEY` в `.env`
  (у пользователя его пока нет); без него тул зарегистрирован и протестирован на fake-сервере,
  но реальный вызов вернёт ошибку авторизации до тех пор, пока ключ не будет добавлен.
