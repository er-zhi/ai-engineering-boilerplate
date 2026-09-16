# Tool Service

Реализует раздел 3 «Tool Service» из `.temp/plans/2026-09-12.md` и раздел «3. Tool Service» из
`.temp/plans/todo.md`. Второй потребитель `engine-core::TaskExecutor` (первый — `LlmTaskExecutor`
в `services/engine`) и первый реальный вызов из встроенного графа `agent` (`engine-core`).

## Что это

Реестр тулов + исполнитель + детерминированная policy по риску. Engine не знает, что за тул
`web_search` — он знает только `kind = "tool"`; Tool Service знает `slug`, что делает по нему, и
с каким риском. Учётные данные наружу не текут: сегодня системные тулы держат свой ключ в `.env`
Tool Service напрямую (Integrations Service не построен — «не сегодня» в `todo.md`), это явно
временно.

## Модель

### `tools`

```rust
struct Tool {
    id: i64,
    user_id: Option<Uuid>,   // None = системный
    slug: String,             // уникален в рамках владельца
    name: String,
    description: String,      // видит LLM через ListTools
    input_schema: Value,      // jsonb, JSON Schema
    output_schema: Value,     // jsonb, JSON Schema
    connection_id: Option<i64>, // не используется сегодня — задел под Integrations
    risk: Risk,                // ReadOnly | Write | Destructive
    timeout_seconds: i32,
    status: Status,            // Draft | Validated | Active
}
```

Уникальность `slug` в рамках владельца — `NULL != NULL` в Postgres ломает обычный
`UNIQUE(user_id, slug)` для системных тулов (две строки с `user_id=NULL` и одинаковым `slug`
прошли бы). Нужен частичный уникальный индекс `UNIQUE(slug) WHERE user_id IS NULL` плюс обычный
`UNIQUE(user_id, slug) WHERE user_id IS NOT NULL`.

### Исполнители системных тулов

- `web_search(query, limit) -> [{title, url, snippet}]` — `SearchProvider` trait,
  `BraveSearchProvider` (reqwest, `BRAVE_SEARCH_API_KEY`). `risk: ReadOnly`.
- `web_fetch(url) -> {title, text}` — reqwest GET + `common::extract::extract(url, html)`.
  Функция переезжает из `services/crawler/src/extract.rs` в `common/src/extract.rs` (второй
  потребитель, по правилу `common/README.md`); crawler переключается на неё же. `risk: ReadOnly`.
- `kb_search(query, page_types?) -> [SearchResult]` — тонкий Connect-клиент к
  `knowledge_base.v1.KnowledgeBaseService/Search` (proto и метод уже существуют). `risk: ReadOnly`.
- `kb_read_document(document_ref) -> {content}` — то же для `ReadDocument`. `risk: ReadOnly`.

Регистрируются при старте `main.rs`, как встроенные графы у Engine — без RPC.

## Policy по риску (детерминированный Rust, не LLM)

```rust
enum Decision { Execute, RequiresApproval }
fn decide(risk: Risk) -> Decision {
    match risk { Risk::ReadOnly => Decision::Execute, _ => Decision::RequiresApproval }
}
```

`Wait::Approval` в Engine сознательно не достроен (как и `Subgraph` — оба в «Вне скоупа»
спека Engine). Поэтому `Execute` на `Write`/`Destructive` тул сегодня не зависает и не
выполняет тайно: возвращает `status = "requires_approval"` без вызова тула. Аудит-таблица для
`Destructive` — не строится сегодня: писать в неё пока некому (ни один вызов не исполняется).
Когда approval-flow появится в Engine, обе половины (policy здесь и `Wait::Approval` там) уже
на своих местах.

### `Execute` на пользовательский тул — чего сегодня нет

`CreateTool`/`ValidateTool`/`ActivateTool` реальны и работают — схема, статус, проверка LLM.
Но у произвольного пользовательского тула нет исполнителя: `connection_id` ни на что не
указывает (Integrations нет), общего механизма «вызвать HTTP по шаблону» этот спек не вводит —
это отдельная, не запрошенная сегодня фича. `Execute` на slug без Rust-реализации (то есть
любой пользовательский тул) возвращает `status = "error"` с понятным сообщением «у
пользовательских тулов пока нет бэкенда исполнения» — не зависает и не делает вид, что
сработало. Тот же приём, что у Engine с `Subgraph`: RPC-поверхность настоящая, исполнение —
явно за скоупом с чёткой ошибкой.

## Connect RPC (`common/proto/tools/v1/tools.proto`)

| Метод | Назначение |
|---|---|
| `CreateTool(name, description, input_schema_json, output_schema_json, risk, timeout_seconds) -> {tool_id}` | `status = Draft`, `user_id` из `Principal` |
| `ValidateTool(tool_id) -> {status, feedback}` | вызывает llm-router (medium tier), проверяет схему/описание на ясность и опасные паттерны; `Draft -> Validated` или остаётся `Draft` с `feedback` |
| `ActivateTool(tool_id) -> {}` | требует `Validated`; `-> Active` |
| `ListTools(user_id? из Principal) -> {tools: [{slug, name, description, input_schema_json}]}` | системные + свои `Active`; это и есть каталог для LLM-промпта |
| `Execute(slug, input_json, idempotency_key) -> {status: "ok"\|"requires_approval"\|"error", output_json, error_message}` | вызывается `ToolTaskExecutor` из Engine |

`user_id` везде — из `Principal` (metadata), никогда из тела запроса, тот же принцип, что уже
в Engine.

## Интеграция с Engine

Две правки в `services/engine`, вне `engine-core`:

1. **`services/engine/src/executors/tool.rs`** — `ToolTaskExecutor { client }`, реализует
   `TaskExecutor`. Конвенция `agent_graph` (`engine-core/src/builder.rs`): tool-узел без
   `config`, вызов читается из `state["llm"]["tool_call"] = {name, args}`. Executor читает это
   через `state.pointer(...)`, зовёт `Execute(slug: name, input: args, idempotency_key)`,
   `Ok`/`requires_approval` маппит в `Value` для `state["tool_result"]` (reducer `Append`, уже
   в `agent_graph`), `error` — в `TaskError::Failed`.
2. **`services/engine/src/dispatch.rs`** — `Dispatcher` получает второе поле `tool:
   ToolTaskExecutor`, `match kind { "llm" => ..., "tool" => self.tool.execute(...), ... }`.

Третья, более глубокая правка — **LLM должен видеть каталог тулов**: `build_prompt`
(`executors/llm.rs`) сегодня не перечисляет доступные тулы, только просит модель ответить JSON.
`Dispatcher` перед вызовом `llm` зовёт `Tool Service.ListTools` и передаёт список в
`build_prompt` (новый параметр) — без этого `agent_graph` структурно рабочий, но модели
неоткуда узнать имя тула.

`user_id` у вызова тула через порт `TaskExecutor::execute(kind, config, state, idempotency_key)`
— порт этого поля не несёт. Сегодня из графов вызываются только системные тулы (`user_id=NULL`,
находятся без контекста пользователя), так что сигнатуру порта `engine-core` трогать не нужно.
Пользовательские тулы по имени из графа — когда Chat даст пользователю на них ссылаться;
тогда порт получит это поле одним небольшим, обратно совместимым расширением. Явно откладываю.

## Конвенции

`services/tool` (package `tool`), entity-first SeaORM, Connect RPC/`buffa` как везде,
`JsonBinary`/`jsonb` без исключений, Dockerfile+compose по образцу `services/engine`,
testcontainers для интеграционных тестов.

## Тесты

- `tools`: уникальность slug (системный vs системный конфликт; системный vs пользовательский с
  тем же slug — не конфликт).
- Policy: `ReadOnly -> Execute`, `Write`/`Destructive -> RequiresApproval`, чистая функция.
- `web_search`/`web_fetch`/`kb_search`/`kb_read_document`: fake HTTP/fake Connect-сервер
  (тот же паттерн, что `llm_router_client.rs`), без реальной сети в юнит-тестах.
- `ValidateTool`: fake llm-router, `Draft -> Validated` и `Draft` (остаётся) с `feedback`.
- `ToolTaskExecutor`: читает `tool_call` из state, зовёт fake Tool Service, три ветки
  (`ok`/`requires_approval`/`error`) маппятся в правильный `NodeOutput`.
- Интеграционный: реальный Postgres (testcontainers) — `CreateTool -> ValidateTool ->
  ActivateTool -> ListTools` видит его; `Execute` на `Write` тул -> `requires_approval`, тул не
  вызван.

## Вне скоупа

- Integrations Service — креды остаются в `.env` Tool Service до его появления.
- Approval-flow (`Wait::Approval` в Engine) и аудит-таблица для `Destructive`.
- Исполнение произвольных пользовательских тулов (см. «`Execute` на пользовательский тул» выше)
  и их вызов из графа по имени (нужно и расширение порта `TaskExecutor`, и сам исполнитель).
- Фронт «Валидировать»/«Активировать» — RPC готовы, UI будет частью Chat Service.
