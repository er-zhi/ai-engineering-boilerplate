# Engine: граф-движок

Реализует раздел 2 «Engine Service» из `.temp/plans/2026-09-12.md` и раздел 13 «Execution Loop»
из `.temp/goal.md` («completion criterion, deadline, budget, лимит итераций, проверка
прогресса, восстановление из сохранённого состояния после сбоя»). Субстрат — тот же, что уже
описан в `docs/superpowers/specs/2026-09-15-session-topics-design.md` для тем сессии:
checkpoint после каждого super-step, `interrupt`/resume. Concept не новый — повторяем
LangGraph-подобный граф-раннер (BSP/Pregel super-step) с нуля на Rust; там, где нужен свой
выбор, используются стандартные механизмы (JSON Pointer, барьерный join), а не изобретаются
новые.

## Что это и чем не является

Engine — тонкий диспетчер над графом, у которого нет собственной памяти между тиками: всё
состояние execution'а — в Postgres. Engine не знает про LLM, тулы, чат, темы, пользователей как
людей — только про узлы, рёбра, state и события. Любой узел, требующий I/O (вызов LLM, тула),
делегируется наружу через один порт; сам Engine исполняет только чистую логику выбора
следующего шага.

Engine не хранит креды, не вызывает внешние API напрямую (`.temp/plans/2026-09-12.md`, раздел
2: «Engine must NOT own credentials or directly call third-party APIs»).

## Модель

### Graph

```rust
struct Graph {
    id: GraphId,
    version: u32,        // растёт при правке; execution привязан к (id, version)
    user_id: Option<UserId>, // None = системный граф
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    entry: NodeId,
}
```

Граф — JSON, хранится в БД, валидируется JSON Schema при регистрации. Правка графа создаёт
новую версию; уже бегущие execution'ы продолжают работать по своей зафиксированной версии —
правка никогда не меняет поведение чужого запущенного execution.

### Node

Закрытый набор из шести видов. Отдельного `Router`/`Conditional` нет — маршрутизация целиком на
`Edge.condition` (см. ниже); отдельного `HumanApproval` нет — это частный случай `Wait`.

```rust
enum Node {
    Task { id: NodeId, kind: String, config: Value },
    FanOut { id: NodeId, source: JsonPointer, item_var: String, target: NodeId },
    FanIn { id: NodeId, key: String, reducer: Reducer }, // key — куда в state пишется свёрнутый массив результатов
    Subgraph { id: NodeId, graph_id: GraphId, version: Option<u32>, input: JsonPointer, output_key: String }, // version: None = последняя на момент старта этого узла, разово зафиксированная в дочернем execution — не «плавающая»
    Wait { id: NodeId, on: WaitKind },
    End { id: NodeId },
}

enum WaitKind {
    Approval { risk: String },      // Tool Service policy решает, попадать ли сюда
    ExternalEvent { key: String },  // произвольное внешнее событие с этим ключом
    UserInput,                      // ждёт новую реплику (interrupt)
    Timer { until: DateTime<Utc> },
}
```

`Task.kind` — строка (`"llm"`, `"tool"`, ...), Engine её не интерпретирует. Кто исполняет какой
`kind`, решает композиция в сервисе `engine`, не сам Engine — добавление нового вида узла не
требует правки `engine-core`. Конкретно: один тип, реализующий порт `TaskExecutor`
(`execute(kind, ...)`), внутри которого — обычный `match kind`; не `HashMap<String, Arc<dyn
TaskExecutor>>` — трейт использует `impl Future<...> + Send` в возвращаемой позиции (без
`async_trait`, см. «Порты»), что несовместимо с `dyn Trait`. Новый вид узла — новая ветка
`match` в этом одном месте, не новая архитектура.

### Edge и Condition

```rust
struct Edge { from: NodeId, to: NodeId, condition: Condition }

enum Condition {
    Always,
    Truthy(JsonPointer),
    Eq(JsonPointer, Value),
    Exists(JsonPointer),
    Failed,               // узел завершился ошибкой
    Not(Box<Condition>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
}
```

`JsonPointer` — RFC 6901 (`serde_json::Value::pointer`), не собственный DSL. После выполнения
узла Engine вычисляет условие каждого исходящего ребра над `State`; **все** рёбра с истинным
условием срабатывают — так один узел с двумя условными рёбрами естественно ветвится, без
отдельного «Router»-узла (то же самое, что conditional edges в LangGraph). Циклы разрешены:
граф не обязан быть DAG.

Если `Task` вернул ошибку и ни одно исходящее ребро не имеет условия `Failed` (или `Not(...)`,
которое его покрывает) — весь execution переходит в `Failed`. Ретраи транзиентных ошибок (сеть,
таймаут провайдера) — не забота Engine: они происходят внутри адаптера, реализующего
`TaskExecutor` (простой retry-with-backoff цикл, см. «Что реально переиспользуется» ниже),
прежде чем адаптер вообще вернёт Engine окончательный успех или неудачу.

### State

`serde_json::Value` плюс декларативные reducers по ключам верхнего уровня, заданные в графе:

```rust
enum Reducer { Replace, Append, Merge }
```

Несколько узлов, завершившихся в одном super-step (в частности — ветви `FanOut`), пишут в разные
или одинаковые ключи без гонок, потому что применение всех результатов к state происходит
последовательно внутри одного `step()`, а не параллельно с БД-конкурентностью. `FanIn.reducer`
(см. «FanOut / FanIn» ниже) — тот же тип: сворачивает результаты ветвей тем же механизмом, что
обычный ключ state, отдельного понятия для join нет.

### Execution

```rust
struct Execution {
    id: ExecutionId,
    graph_id: GraphId,
    graph_version: u32,
    user_id: Option<UserId>,
    status: Status,
    current_nodes: Vec<NodeId>,
    state: Value,
    iteration: u32,
    max_iterations: u32,
    deadline: Option<DateTime<Utc>>,
    budget: Budget,           // тип из этого же crate'а — токены/tool-вызовы/wall-time
}

// Поля — remaining, не лимиты: сколько ещё можно потратить. Создаётся из конфигурации графа
// (`Budget::new(limits)`), charge() насыщающе вычитает, exhausted() — true, когда любое поле
// достигло нуля.
struct Budget { tokens_remaining: u32, tool_calls_remaining: u32, wall_time_remaining: Duration }
impl Budget {
    fn charge(&mut self, tokens: u32, tool_calls: u32, wall_time: Duration) { /* saturating_sub по каждому полю */ }
    fn exhausted(&self) -> bool { self.tokens_remaining == 0 || self.tool_calls_remaining == 0 || self.wall_time_remaining.is_zero() }
}

enum Status {
    Ready,
    Running,             // тик выполняется прямо сейчас (lease удержан)
    Waiting(WaitKind),
    Completed,
    Failed,
    Cancelled,
}
```

### Checkpoint и ExecutionEvent

Оба типа — часть `engine-core` целиком (не переиспользуют ничего из `common`, см. «Что реально
переиспользуется» ниже): `Checkpoint` несёт свою версию схемы прямой константой, апгрейдить
пока не с чего — общий `Versioned<T>`-обёртка была бы преждевременной генерализацией ради
одного случая.

```rust
const CHECKPOINT_SCHEMA_VERSION: u16 = 1;
struct Checkpoint { schema_version: u16, execution_id: ExecutionId, step: u32, state: Value, current_nodes: Vec<NodeId> }

struct Event<P> { id: Uuid, version: u16, occurred_at: DateTime<Utc>, user_id: Option<UserId>, correlation_id: ExecutionId, causation_id: Option<Uuid>, payload: P }
type ExecutionEvent = Event<ExecutionPayload>;
enum ExecutionPayload {
    ExecutionStarted,
    NodeStarted { node: NodeId },
    NodeCompleted { node: NodeId, output: Value },
    NodeFailed { node: NodeId, error: String },
    TaskOutput { node: NodeId, chunk: Value },  // стрим (например, токены LLM)
    Waiting { on: WaitKind },
    Resumed,
    Interrupted,
    ExecutionCompleted { final_state: Value },
    ExecutionFailed { error: String },
    ExecutionCancelled,
}
```

`correlation_id` = `execution_id`, `causation_id` — id события-причины, если применимо (например,
`NodeStarted` дочернего execution причинно связан с `NodeStarted` узла `Subgraph` родителя).
Единственный публичный контракт, на который смотрят все клиенты (Chat, MCP, Evals), — не сам
этот Rust-тип, а его proto-отражение в `common/proto/engine/v1` (см. «Что реально
переиспользуется» ниже); события тем из спека `session-topics-design.md` — проекция поверх
этого потока, не отдельный лог.

## `step()` — вся логика в одной чистой функции

```rust
fn step(graph: &Graph, execution: Execution, outputs: Vec<NodeOutput>) -> (Execution, Vec<ExecutionEvent>)
```

Без `async`, без I/O, без БД. Берёт результаты уже выполненных узлов (`outputs` — по одному на
элемент `current_nodes`, либо `Err` для отказа), применяет их к `state` через reducers,
списывает `Budget`, проверяет `max_iterations`/`deadline`, вычисляет условия исходящих рёбер,
формирует новый `current_nodes` (может быть пустым → `Completed`, если среди сработавших рёбер
есть путь в `End`; может содержать `Wait`-узел → `Waiting`). Это единственное место, где
описана семантика графа — весь остальной код (`engine`-сервис) вокруг него — планировщик и
персистентность.

### FanOut / FanIn

`FanOut` читает массив по `source` в `state`, порождает по экземпляру узла `target` на элемент,
каждому — свою копию state с `item_var`, добавленным в неё. `FanIn` — барьерный join: ждёт все
экземпляры, порождённые соответствующим `FanOut` (число — длина массива на момент спавна), и
сворачивает их результаты через `reducer`. Один режим ожидания (все-до-одного), без частичных
N-из-M — не нужен ни тестам, ни сегодняшнему сценарию.

### Subgraph

Узел `Subgraph` не разворачивается инлайн. При его исполнении Engine стартует отдельный дочерний
`Execution` (`graph_id`, зафиксированная `version`) со своей цепочкой checkpoint'ов;
`causation_id` его событий = id события `NodeStarted` родителя. Родительский execution в этот
момент уходит в `Waiting(ExternalEvent{key: child_execution_id})`. Когда дочерний доходит до
`End`, его финальный `state` вливается в state родителя по ключу `output_key` через обычный
reducer, и родитель пробуждается тем же механизмом, что и любое внешнее событие. Это тот же
паттерн, что «результат темы вливается в фокусную» в `session-topics-design.md` — агент как
подграф использует ровно тот же механизм, что тема как execution.

## Порты

```rust
trait TaskExecutor: Send + Sync {
    fn execute(&self, kind: &str, config: &Value, state: &Value, idempotency_key: &str)
        -> impl Future<Output = Result<Value, TaskError>> + Send;
}
trait CheckpointStore: Send + Sync { /* load/save Checkpoint */ }
trait EventSink: Send + Sync { /* append ExecutionEvent */ }
```

Стиль — как существующий `common::cache::CacheStore` (`impl Future<...> + Send`, без
`async_trait`). `TaskExecutor` — один трейт на весь Engine; диспетчеризация по `kind` — забота
композиции в `main.rs` сервиса, не самого порта. `idempotency_key` = `(execution_id, node_id,
step)` — часть контракта трейта с первого дня, потому что Engine гарантирует **at-least-once**
на узел (прерванный посередине узел при resume выполняется заново с начала): исполнитель с
побочным эффектом (запись, внешний вызов) обязан использовать этот ключ для дедупликации.
Хранилище дедупликации — не забота Engine и не общий модуль сегодня: ни один исполнитель этого
плана (LLM-адаптер) не имеет разрушительных побочных эффектов, повтор которых нужно гасить —
`idempotency_key` передаётся и логируется, но ничем пока не проверяется. Когда появится
исполнитель с side-effects (Tool Service, Write/Destructive-тулы), там же появится и хранилище
дедупликации; переезд в `common` — только если им реально понадобится делиться со вторым
потребителем, а не заранее.

Fake-реализации всех трёх портов — за feature `test-support`, как у остальных сервисов.

## Runtime сервиса `engine` — event loop на tokio

Без state в процессе: между тиками движок ничего не помнит, поэтому любое число инстансов
взаимозаменяемо и может обслуживать executions любых клиентов.

1. **Lease.** Модуль `lease` внутри крейта `engine` (не `common` — см. «Что реально
   переиспользуется» ниже) берёт один `Ready` execution: `SELECT ... FOR UPDATE SKIP LOCKED`
   + `lease_until` (истёкшая аренда — значит инстанс упал — возвращает execution в `Ready`).
   Heartbeat, продлевающий аренду при долгом тике, пока не реализован: аренда фиксированная и
   должна быть длиннее самого долгого узла; см. комментарий в `tick.rs`.
2. **Загрузка.** Последний `Checkpoint` из `CheckpointStore`.
3. **Исполнение узлов.** Для каждого `current_nodes` — если управляющий узел (`FanIn`, `End`) —
   решается синхронно; если `Task`/`FanOut`/`Subgraph` — асинхронный вызов через
   `TaskExecutor`, все параллельно в одном `tokio::task::JoinSet`.
4. **`step()`.** Чистая функция превращает `(graph, execution, outputs)` в
   `(execution', events)`.
5. **Фиксация.** Одна SQL-транзакция пишет обновлённый `executions` (статус, `current_nodes`,
   снятая аренда), новый `checkpoints`-ряд и `execution_events`-ряды, и последней инструкцией —
   `NOTIFY engine_tick`. Отдельная таблица-outbox не нужна: `NOTIFY`, выполненный внутри
   транзакции, доставляется слушателям Postgres тогда и только тогда, когда эта транзакция
   зафиксирована (`COMMIT`) — при `ROLLBACK` уведомление не уходит вообще. Это ровно свойство,
   которое паттерн transactional outbox эмулирует поверх нетранзакционного брокера; здесь
   база одна, и это свойство даёт сам Postgres.
6. **Освобождение аренды.** Новый статус: `Ready` (есть что делать дальше — можно тут же
   вернуть в очередь) / `Waiting` / `Completed` / `Failed`.

Пробуждение: `tokio::select!` на подписке `LISTEN engine_tick` плюс `tokio::time::interval` как
fallback (на случай пропущенного уведомления — соединение переподключилось, гонка).
`tokio::sync::Semaphore` — единственный лимит одновременно исполняемых execution-тиков на
инстанс; никакой отдельной очереди задач поверх БД, никаких actor-фреймворков.

### Восстановление после падения

Инстанс падает в середине тика → его lease на затронутый execution истекает по времени →
другой (или тот же после рестарта) инстанс подхватывает execution в статусе, в котором он был
до начала того тика (последний зафиксированный checkpoint), и узел, который тик не успел
доисполнить, выполняется заново — то самое at-least-once. Никакого отдельного recovery-пути не
существует: восстановление — это просто ещё один обычный тик.

## Interrupt / Resume

- `Interrupt(execution_id, input)` — атомарно: записать `input` в state (или в отдельный буфер
  входящих реплик, который следующий тик применит), выставить `Ready`, `NOTIFY`. Если
  execution сейчас `Waiting(UserInput)` — это штатное продолжение; если он был в середине
  другой работы — текущий выполняющийся узел будет повторён заново на следующем resume (см.
  контракт at-least-once), это и есть цена мгновенного прерывания без отдельного «сохранить и
  восстановить промежуточное состояние узла».
- `Resume(execution_id, event)` — то же самое для `Wait::ExternalEvent`/`Wait::Approval`: строка
  в БД с ключом ожидания, `Ready`, `NOTIFY`.

## Лимиты

`Budget` (см. «Execution» выше — `tokens_remaining`/`tool_calls_remaining`/
`wall_time_remaining`, `charge()`/`exhausted()`) списывается в `step()` из `outputs` (например,
`usage.total_tokens` в ответе LLM-узла), не самими узлами. `max_iterations`
— счётчик super-step'ов, растёт в `step()`, при превышении — `Failed` с понятной причиной вместо
тихого зависания. `deadline` проверяется тем же тиком через текущее время, переданное снаружи
(не читается изнутри `step()` — время передаётся аргументом, чтобы функция оставалась чистой и
тестируемой без реального часов).

## gRPC-поверхность (Connect RPC, `common/proto/engine/v1`)

| Метод | Назначение |
|---|---|
| `RegisterGraph(graph) -> {graph_id, version}` | сохранить/обновить граф |
| `StartExecution(graph_id, version?, input) -> {execution_id}` | версия по умолчанию — последняя на момент вызова |
| `Interrupt(execution_id, input) -> {}` | реплика/новый вход в бегущий execution |
| `Resume(execution_id, wait_key, event) -> {}` | внешнее событие снимает `Wait` |
| `Approve(execution_id) / Reject(execution_id, reason)` | снимает `Wait::Approval` |
| `Cancel(execution_id) -> {}` | `Cancelled`, без доисполнения |
| `GetExecution(execution_id) -> Execution` | текущее состояние + `current_nodes` (для рендера графа на фронте) |
| `StreamEvents(execution_id \| user_id) -> stream ExecutionEvent` | server-stream через bounded channel (тот же урок backpressure, что у crawler'а — отстающий клиент не растит память сервера) |

`Principal` (см. ниже) приходит в metadata запроса, не в теле; `user_id` пишется в `executions`
и `graphs` из него, не из аргумента, который клиент мог бы подделать.

## Схема БД (`engine`)

- `graphs(id, version, user_id NULL=системный, definition jsonb, created_at)` — уникальность
  `(id, version)`.
- `executions(id, graph_id, graph_version, user_id, status, current_nodes jsonb, iteration,
  max_iterations, deadline, budget jsonb, lease_owner, lease_until, created_at, updated_at)`.
  `current_nodes` здесь — денормализованная копия последнего checkpoint'а: пишется в той же
  транзакции, что и checkpoint, только чтобы `GetExecution` (его дёргает фронт при каждом рендере
  графа темы) не требовал джойна с `checkpoints`. `state` в `executions` **не хранится** — он
  живёт только в `checkpoints`, чтобы не дублировать потенциально большой JSON; собранный в
  памяти `Execution` (Rust-тип из раздела «Execution» выше) — это строка `executions` плюс
  `state`/`current_nodes` последнего `checkpoints`-ряда, соединённые при загрузке, не два
  независимых источника истины.
- `checkpoints(execution_id, step, schema_version, state jsonb, current_nodes jsonb,
  created_at)`, PK `(execution_id, step)` — источник истины для `state`.
- `execution_events(id, execution_id, user_id, version, causation_id, occurred_at, payload
  jsonb)`, append-only, индекс по `execution_id, id` для `StreamEvents`.
- Индекс под lease-запрос (см. «Runtime» п. 1): частичный индекс
  `executions (updated_at) WHERE status IN ('ready', 'running')` — только «горячие» строки.
  Без него claim — это seq scan по всей `executions`, и он деградирует ровно с ростом
  завершённых строк, которые ему вообще не нужны. Создаётся так же, как индекс
  `execution_events`: строкой `CREATE INDEX IF NOT EXISTS` после schema-sync.

Таблицы делятся на **горячие** и **холодные** — см. «Горячие и холодные данные» ниже:
`executions` и `checkpoints` — рабочая область движка, там живёт только то, что ещё
исполняется; `execution_events` и `graphs` — постоянный лог и справочник.

Entity-first через SeaORM schema-sync, как у остальных сервисов — таблицы создаются на старте
из entity, отдельных `.sql`-миграций нет.

## Горячие и холодные данные, ретеншн

Движок ходит в Postgres на каждом тике, поэтому важно, чтобы рабочие таблицы оставались
маленькими и не росли с историей. Правило: **горячие таблицы хранят только то, что ещё
исполняется; всё, что нужно для анализа, уже лежит в `execution_events`.**

| Таблица            | Роль                         | Что в ней после завершения execution |
|--------------------|------------------------------|--------------------------------------|
| `executions`       | очередь + текущая позиция    | ничего — строка удаляется            |
| `checkpoints`      | источник истины для `state`  | ничего — все шаги удаляются          |
| `execution_events` | append-only лог для анализа  | всё, навсегда (или по своему, отдельному ретеншну) |
| `graphs`           | справочник определений       | не трогается                         |

Почему ничего не теряется: `ExecutionCompleted { final_state }` / `ExecutionFailed { error }` /
`ExecutionCancelled` — последнее событие каждого execution — уже несёт итог; промежуточные
состояния восстанавливаются из `NodeCompleted { output }` по порядку `id`. Промежуточные
`checkpoints` — это не история, а точка рестарта: после терминального статуса они нужны ровно
никому.

Механика:

- **Терминальный статус ≠ немедленное удаление.** После `Completed`/`Failed`/`Cancelled` строка
  живёт ещё `ENGINE_TERMINAL_RETENTION` (по умолчанию 1 час): клиент, который только что вызвал
  `RunGraph`, всё ещё дёргает `GetExecution` за результатом. По истечении срока `executions` и
  `checkpoints` для этого id удаляются одной транзакцией.
- **Кто удаляет.** Тот же event loop `wakeup`: на каждом срабатывании fallback-интервала (не
  на каждом `NOTIFY`) один `DELETE ... WHERE status IN (...) AND updated_at < now() - $retention
  LIMIT`-батч по `executions` с каскадом на `checkpoints`. Отдельного крона/сервиса нет — это
  такой же дешёвый шаг цикла, как `claim_one_ready`. Батч ограничен (например, 500 строк), чтобы
  не держать долгую транзакцию рядом с горячими `SKIP LOCKED`-запросами.
- **`GetExecution` после очистки** отвечает `NotFound`. Клиент, которому нужен итог позже
  часа, читает `StreamEvents` (события живут) — это и есть его контракт для истории, а не
  `GetExecution`.
- **`execution_events` растёт неограниченно** — это осознанно: аналитический лог, а не рабочая
  область, его чтения индексированы по `execution_id` и не мешают тикам. Поэтому таблица
  **партиционирована по `occurred_at` помесячно с первой версии** (правило
  `gate-database` → «Growth»): ретеншн — это `DROP PARTITION`, а не `DELETE` по миллионам строк,
  и переделывать заполненную таблицу под партиции потом — это полный rewrite под локом. PK
  становится `(occurred_at, id)`, уникальность `event_id` — тоже с `occurred_at` в ключе;
  `StreamEvents` читает по `execution_id, id` внутри партиций, для живого execution это
  одна-две партиции.
- **`checkpoints` во время исполнения** хранятся все, ограничены `max_iterations` — они полезны
  при отладке зависшего execution и стоят дёшево, пока execution жив. Хранить только последний —
  преждевременная оптимизация; при необходимости включается тем же sweep'ом.

### `NOTIFY` и число инстансов

`NOTIFY engine_tick` без payload будит **каждый** инстанс, и каждый делает ровно один
`claim_one_ready`. При N инстансах одно событие даёт N запросов, из которых N−1 возвращают
пусто; `SKIP LOCKED` делает их дешёвыми, поэтому до ~десятка инстансов это не мера. Дальше —
`NOTIFY engine_tick, '<execution_id>'` и claim по конкретному id первым, общий claim — только по
fallback-интервалу. Это изменение локально в `wakeup`/`lease`, схема и ядро не меняются.

### Когда Postgres перестанет хватать

Профиль нагрузки движка: одна короткая транзакция на тик (миллисекунды) против ожидания LLM
(секунды). Один Postgres с индексом выше и чистыми горячими таблицами держит порядка тысяч
тиков в секунду. Если понадобится больше — из Postgres уезжает **только очередь**
(`claim`/`NOTIFY` → Redis/NATS за трейтом, см. `docs/architecture.md`), но не `checkpoints`:
они — источник истины для восстановления после падения и должны фиксироваться транзакционно
вместе со статусом.

## Встроенные графы

`GraphBuilder` — Rust-код, порождающий тот же JSON, что принял бы `RegisterGraph` (никакого
второго формата для «системных» графов):

- `simple`: `START(entry=llm) → llm → End`.
- `rag`: `START(entry=kb_search) → kb_search → llm → End`.
- `agent`: `START(entry=llm) → llm →[Truthy(/tool_call)] tool → llm`, `llm →[Not(Truthy(/tool_call))] End`,
  `max_iterations` по умолчанию — граница от зацикливания. Сегодняшний сквозной сценарий бежит
  на этом графе.

## Principal и `user_id` — что уже есть в проекте и чего нет

В `services/gateway` сегодня один общий пароль (`services/gateway/src/auth.rs`), сессия — opaque
токен в `gateway.sessions` через `CacheStore` (`services/gateway/src/sessions.rs`), без понятия
отдельных учётных записей. «Пользователь A не видит данные пользователя B» из
`.temp/plans/todo.md` (раздел 5) сегодня буквально нечем проверить — учётных записей нет.

Решение на сегодня: `user_id` — не учётная запись, а стабильный UUID, который Gateway выдаёт
сессии при её создании (столбец рядом с существующей строкой сессии) и кладёт в
`Principal{user_id, session_id}` в metadata каждого исходящего Connect-запроса. «Два
пользователя» в тесте изоляции — это два разных браузерных/curl-сеанса, у каждого свой
`user_id`, не два разных человека с разными паролями. Полноценные множественные учётные записи
(свои пароли/OAuth на человека) — не сегодня, ниже.

Модуль `principal` внутри крейта `engine` (не `common`, см. ниже): `Principal { user_id: Uuid,
session_id: String }` + `from_metadata(&Metadata) -> Option<Principal>`, читает два заголовка
Connect-запроса. **Кто их ставит — вне этого спека.** Gateway сегодня не проксирует Engine
вообще (Chat ещё не построен), поэтому write-сторона (interceptor на Gateway, генерация
`user_id` при создании сессии) — задача плана Chat Service, когда Gateway впервые начнёт
проксировать Engine. Здесь и сейчас smoke-тест сам кладёт эти два заголовка в curl-запрос,
имитируя то, что позже будет делать Gateway — контракт (Engine доверяет только metadata,
никогда телу запроса) соблюдён с первого дня, даже когда писать метаданные пока некому кроме
теста.

## Что реально переиспользуется, а что — нет

`common/README.md` уже формулирует правило: значение, пересекающее границу сервиса, живёт в
**proto**-контракте этого сервиса; код попадает в `common` только когда минимум два сервиса
используют одно и то же *поведение* (не данные). Исходная версия этого спека закладывала
`common::events`/`outbox`/`lease`/`idempotency`/`versioned`/`budget`/`principal`/`error`/
`resilience` — почти все они на самом деле нужны только Engine, а межсервисный обмен данными
(Chat читает события Engine) и так идёт через proto, ровно как Gateway сегодня получает типы
Crawler и Knowledge Base. Ревизия:

| Было задумано | Реальных потребителей сегодня | Решение |
|---|---|---|
| `common::events::Event<P>` | 1 (Engine) — Chat читает события через сгенерированный proto-клиент `common/proto/engine/v1`, не через общий Rust-тип | не нужен: proto — это и есть контракт |
| `common::outbox` | 0 — Postgres `NOTIFY` уже транзакционен | не нужен вообще, см. «Runtime» выше |
| `common::lease` | 1 (Engine); миграция `crawler.crawl_jobs` на этот паттерн — явно «не сегодня» | модуль `engine::lease`, переедет в `common`, когда появится второй реальный потребитель |
| `common::idempotency` | 0 — нет исполнителя с side-effects (Tool Service не построен) | не строим; сигнатура трейта место под него держит (см. «Порты») |
| `common::versioned` | 1 (checkpoint), апгрейдить пока не с чего | обычная колонка `schema_version SMALLINT` + константа, без общего generic-типа |
| `common::budget` | 1 (Engine); llm-router уже считает лимиты по-своему (`common::llm`), обобщать его — не сегодня | тип внутри `engine-core` |
| `common::principal` | 1 (Engine); Gateway ещё не пишет заголовки | модуль `engine::principal`, переедет в `common`, когда Tool/Chat будут делать то же самое |
| `common::error` | 0 — каждый сервис (включая уже существующие) маппит ошибки сам, это статус-кво, не регресс | локальная маппа `EngineError -> ConnectError` |
| `common::resilience` | 1 (Engine → llm-router) | простой retry-with-backoff внутри LLM-адаптера |

Итог: этот спек не добавляет новых модулей в `common` — только новый proto-контракт
(`common/proto/engine/v1/engine.proto`), как и любой предыдущий сервис. Любая из строк выше
переезжает в `common` в тот день, когда для неё действительно появляется второй потребитель —
не раньше.

## Тесты (`engine-core`, fake-порты, без Postgres и без LLM)

- Линейный граф `Task → End`.
- Условное ветвление: одно ребро истинно — один путь; оба истинны — оба пути активируются.
- Цикл: граф без `End` упирается в `max_iterations`, статус `Failed` с понятной причиной.
- `FanOut`/`FanIn`: массив из 3 элементов → 3 параллельных узла → барьер → один reducer-результат.
- `Subgraph`: родитель уходит в `Waiting`, дочерний доходит до `End`, результат вливается по
  `output_key`, родитель возвращается в `Ready`.
- `Wait::UserInput` блокирует, `Interrupt` снимает блокировку.
- `interrupt` посреди выполнения узла → следующий resume выполняет этот узел заново (проверка
  контракта at-least-once) с тем же итоговым state, что при однократном безошибочном проходе.
- `deadline`/`budget`: превышение переводит execution в `Failed`, а не зависает.
- Property-тест (как в `knowledge-base`): после произвольной последовательности `step()`-вызовов
  выполненная последовательность `ExecutionEvent` детерминированно восстанавливает финальный
  `Execution` — «ничего не потеряно между checkpoint'ами».

## Вне скоупа этого спека

- Retries/timeouts как встроенная в `engine-core` логика — они снаружи, в адаптерах
  `TaskExecutor` (см. «Edge и Condition» выше).
- Полноценные учётные записи (множественные пароли/OAuth per human) — только session-scoped
  `user_id` сегодня.
- Визуальный редактор графов — графы задаются `GraphBuilder`/JSON.
- N-из-M частичный join в `FanIn` — только барьер «все».
- Integrations Service, пользовательские тулы, Tool/Chat-сервисы — отдельные спеки/планы,
  потребители Engine через описанный выше gRPC-контракт.
