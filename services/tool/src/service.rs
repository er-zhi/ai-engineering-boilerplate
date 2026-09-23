// The tool lifecycle — create, validate, activate, list — and Execute, which runs one.

use chrono::Utc;
use common::proto::llm_router::v1::{
    Answer, DecideRequest, Noul, Question, SystemOneServiceClient, answer::Answer as Given,
};
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{ConnectError, ErrorCode, Protocol};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, ExprTrait,
    QueryFilter,
};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

use crate::entity::tool::{ActiveModel, Entity, Risk, Status};
use crate::error::ToolError;
use crate::providers::{AnySearchProvider, SearchProvider};
use crate::tools::kb_client::KnowledgeBaseClient;

/// One round trip to `Decide`, end to end. Measured in this deployment, `Decide`'s p50 is about
/// 155 ms against `Complete`'s p50 of about 2000 ms (`llm_router.decisions.latency_ms` and
/// `llm_router.requests.latency_ms`) — the same measurement `services/chat/src/intent.rs`'s and
/// `services/engine/src/executors/llm.rs`'s `DECIDE_CALL_TIMEOUT` cite.
///
/// This caller's deadline is deliberately longer than either of those: a human submits a tool
/// definition once, interactively, and would rather wait than have a slow vendor fail the
/// submission outright, and — unlike Chat's turn or the engine's fast-dispatch path, which both
/// have somewhere to fall back to — this path has no fallback if `Decide` fails: `decide_call_error`
/// just turns it into a refusal. 10 s covers a cold connection and this service's own hop out to
/// llm-router and back with room to spare, while one human waits on one submission.
///
/// **Must stay strictly above `services/llm-router`'s `adapters::system_one::REQUEST_TIMEOUT`
/// (1.5 s and shared by every caller of that adapter, this one included).** This client's deadline
/// is asserted as the `grpc-timeout` header llm-router's connectrpc server parses on receipt, and
/// that wraps the *entire* dispatch to the vendor — including the adapter's own inner timeout —
/// in a deadline that starts strictly before the adapter's own clock does. Equal or shorter and
/// this outer deadline always wins that race, dropping llm-router's decision future (and the
/// audit row its synchronous write would have produced) before the inner timeout ever gets to
/// finish and report cleanly; a whole-branch review traced exactly that bug for Chat's matching
/// pair, fixed by keeping the inner strictly below the outer — see
/// `services/llm-router/src/adapters/system_one/mod.rs`'s `REQUEST_TIMEOUT` doc comment. 10 s
/// leaves generous margin here.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

// A wrong "yes" ships an unreviewed tool definition; a wrong "no" only costs a resubmission. 0.7 leans toward
// the cheaper mistake without demanding near-certainty.
const APPROVAL_THRESHOLD: f64 = 0.7;

/// One reviewable fact about a submitted tool definition. `id` is what a `Decide` answer is keyed
/// by; `instructions` is the question asked; `when_true`/`when_false` are what "true" and "false"
/// mean to the model, since a `noul`'s calibration comes from those two sentences, not from the id.
/// `when_false` doubles as the sentence shown to the submitter when this criterion is the reason
/// their tool was refused — see `compose_refusal` — so it is written to read as a reason, not just
/// a model instruction.
#[derive(Clone, Copy, Debug)]
struct Criterion {
    id: &'static str,
    instructions: &'static str,
    when_true: &'static str,
    when_false: &'static str,
}

const INPUT_SCHEMA_CRITERION: Criterion = Criterion {
    id: "input_schema",
    instructions: "Is input_schema a plausible JSON Schema object for what this tool takes as input?",
    when_true: "input_schema is a plausible JSON Schema object describing the tool's input",
    when_false: "input_schema is missing, not an object, or not a plausible JSON Schema for this tool's input",
};

const OUTPUT_SCHEMA_CRITERION: Criterion = Criterion {
    id: "output_schema",
    instructions: "Is output_schema a plausible JSON Schema object for what this tool returns?",
    when_true: "output_schema is a plausible JSON Schema object describing the tool's output",
    when_false: "output_schema is missing, not an object, or not a plausible JSON Schema for this tool's output",
};

const DESCRIPTION_PURPOSE_CRITERION: Criterion = Criterion {
    id: "description_purpose",
    instructions: "Does the description clearly state what the tool does?",
    when_true: "the description clearly states what the tool does",
    when_false: "the description is missing, vague, or does not say what the tool does",
};

/// Only asked, and only required, for `Risk::Write` and `Risk::Destructive` — see `criteria_for`.
/// A read-only tool has nothing to change, so asking this of one would be a question with no true
/// answer available to the model, not a stricter check.
const DESCRIPTION_CHANGES_CRITERION: Criterion = Criterion {
    id: "description_changes",
    instructions: "Does the description state what this tool changes when it runs?",
    when_true: "the description states what this tool changes when it runs",
    when_false: "the description does not say what this tool changes when it runs",
};

const TIMEOUT_CRITERION: Criterion = Criterion {
    id: "timeout",
    instructions: "Is timeout_seconds plausible for the work this tool does?",
    when_true: "timeout_seconds is a plausible amount of time for this tool's work",
    when_false: "timeout_seconds is implausible for this tool's work — too short to complete it, or far longer than it should need",
};

/// Every criterion a tool definition is checked against, one `noul` each, all asked in the same
/// `Decide` call — the vendor evaluates each question in isolation against the same state, so this
/// costs one round trip, not five. `description_changes` is included only when it has an answer to
/// give: see its own doc comment.
fn criteria_for(risk: Risk) -> Vec<Criterion> {
    let mut criteria = vec![
        INPUT_SCHEMA_CRITERION,
        OUTPUT_SCHEMA_CRITERION,
        DESCRIPTION_PURPOSE_CRITERION,
        TIMEOUT_CRITERION,
    ];
    if matches!(risk, Risk::Write | Risk::Destructive) {
        criteria.push(DESCRIPTION_CHANGES_CRITERION);
    }
    criteria
}

pub const MAX_SLUG_CHARS: usize = 64;
pub const MAX_NAME_CHARS: usize = 128;
pub const MAX_DESCRIPTION_CHARS: usize = 8192;
pub const MIN_TIMEOUT_SECONDS: i32 = 1;
pub const MAX_TIMEOUT_SECONDS: i32 = 300;
const DEFAULT_TOOL_HTTP_TIMEOUT: Duration = Duration::from_secs(MAX_TIMEOUT_SECONDS as u64);

fn tool_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().timeout(DEFAULT_TOOL_HTTP_TIMEOUT)
}

/// The guarded client behind `get_guarded`: `self.http`, `web_fetch`'s `fetch`/`fetch_richest`, and a
/// declarative source's `ask` all send through this one, and their whole design assumes it is handed
/// each 3xx hop raw so `get_guarded` can re-check the SSRF guard against the next target itself —
/// under the default policy reqwest would already have followed the hop before this code ever saw
/// it. Search providers do **not** use this client; see `redirect_following_http_client` for why.
pub fn bounded_http_client() -> Result<reqwest::Client, String> {
    tool_http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not build the outbound http client: {error}"))
}

/// The client behind `AnySearchProvider`. A search vendor's own redirects (an http→https upgrade, a
/// moved path, a CDN hop) are not requests to an address this process chose, so there is nothing for
/// an SSRF guard to re-check on each hop — reqwest is left to follow them itself, the way any HTTP
/// client normally would. Handing a search provider the no-redirect `bounded_http_client` instead
/// turns a vendor's ordinary 301/308 into a hard failure of the whole tool call.
pub fn redirect_following_http_client() -> Result<reqwest::Client, String> {
    tool_http_client_builder()
        .build()
        .map_err(|error| format!("could not build the outbound http client: {error}"))
}

#[derive(Debug)]
pub struct NewTool {
    slug: String,
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
    risk: Risk,
    timeout_seconds: i32,
}

impl NewTool {
    pub fn checked(
        slug: String,
        name: String,
        description: String,
        input_schema: Value,
        output_schema: Value,
        risk: Risk,
        timeout_seconds: i32,
    ) -> Result<Self, ToolError> {
        bounded("slug", &slug, MAX_SLUG_CHARS)?;
        bounded("name", &name, MAX_NAME_CHARS)?;
        bounded("description", &description, MAX_DESCRIPTION_CHARS)?;
        if !input_schema.is_object() || !output_schema.is_object() {
            return Err(ToolError::InvalidRequest(
                "input_schema and output_schema must be JSON objects".to_owned(),
            ));
        }
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
            return Err(ToolError::InvalidRequest(format!(
                "timeout_seconds must be between {MIN_TIMEOUT_SECONDS} and {MAX_TIMEOUT_SECONDS}, got {timeout_seconds}"
            )));
        }
        Ok(Self {
            slug,
            name,
            description,
            input_schema,
            output_schema,
            risk,
            timeout_seconds,
        })
    }
}

fn bounded(field: &str, value: &str, max_chars: usize) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::InvalidRequest(format!("{field} is required")));
    }
    let length = value.chars().count();
    if length > max_chars {
        return Err(ToolError::InvalidRequest(format!(
            "{field} is {length} characters, the maximum is {max_chars}"
        )));
    }
    Ok(())
}

pub struct Service {
    db: DatabaseConnection,
    decider: SystemOneServiceClient<HttpClient>,
    search: AnySearchProvider,
    http: reqwest::Client,
    kb: KnowledgeBaseClient,
}

impl Service {
    pub fn new(
        db: DatabaseConnection,
        llm_router_url: &str,
        you_api_key: String,
        brave_api_key: String,
        knowledge_base_url: &str,
    ) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        let http = bounded_http_client()?;
        let client_config = ClientConfig::new(target)
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(VALIDATE_TIMEOUT)
            .proto();
        Ok(Self {
            db,
            decider: SystemOneServiceClient::new(HttpClient::plaintext_http2_only(), client_config),
            search: AnySearchProvider::from_api_keys(
                you_api_key,
                brave_api_key,
                redirect_following_http_client()?,
            ),
            http,
            kb: KnowledgeBaseClient::new(knowledge_base_url)?,
        })
    }

    pub async fn create_tool(
        &self,
        user_id: Option<Uuid>,
        new_tool: NewTool,
    ) -> Result<i64, ToolError> {
        if claims_reserved_slug(user_id, &new_tool.slug) {
            return Err(ToolError::InvalidRequest(format!(
                "slug {:?} is reserved for a system tool",
                new_tool.slug
            )));
        }
        let now = Utc::now();
        let row = ActiveModel {
            user_id: Set(user_id),
            slug: Set(new_tool.slug),
            name: Set(new_tool.name),
            description: Set(new_tool.description),
            input_schema: Set(new_tool.input_schema),
            output_schema: Set(new_tool.output_schema),
            connection_id: Set(None),
            risk: Set(new_tool.risk),
            timeout_seconds: Set(new_tool.timeout_seconds),
            status: Set(Status::Draft),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(row.id)
    }

    // One `Decide` call, one `noul` per criterion, and the verdict is ANDed together in code
    // rather than also asked of the model as its own `meets_standard` gate. Two reasons: a
    // derived gate cannot disagree with the reasons it hands the submitter (an overall-gate noul
    // is one more calibrated guess, and nothing stops the model landing on a different verdict
    // than its own per-criterion answers would imply), and `description_changes` only applies to
    // some tools — a single overall question would have to somehow encode that conditionality
    // into one instruction, where `criteria_for` just decides in Rust which questions exist at
    // all. The cost is that a submitter's threshold moved slightly (previously one 0.7 call, now
    // every one of up to five must each individually clear 0.7), which is the right direction: it
    // was always meant to gate on the conjunction of these facts, not on the model's own summary
    // of them.
    pub async fn validate_tool(
        &self,
        tool_id: i64,
        user_id: Uuid,
    ) -> Result<(bool, String), ToolError> {
        let tool = self.user_owned_tool(tool_id, user_id).await?;
        if already_reviewed(tool.status) {
            return Err(ToolError::InvalidRequest(format!(
                "tool {tool_id} is not Draft"
            )));
        }

        let criteria = criteria_for(tool.risk);
        let mut request: DecideRequest = serde_json::from_value(serde_json::json!({
            "state": definition_text(&tool),
        }))
        .map_err(|e| ToolError::InvalidRequest(format!("could not build the decision: {e}")))?;
        request.questions = criteria
            .iter()
            .map(criterion_question)
            .collect::<Result<_, _>>()?;

        let decided = self
            .decider
            .decide(request)
            .await
            .map_err(decide_call_error)?
            .into_owned();

        // Fail closed by construction, not by special-casing: a criterion we asked about but got
        // no matching `noul` back for — because the decision carried no answers at all, the
        // answer came under an id we never asked, or it came back as a `choice`/`score` instead of
        // a `noul` — simply never matches `Given::Noul(_)` below and so counts as unmet, the same
        // as an explicit "no". None of those situations can produce an approval.
        let failed: Vec<Criterion> = criteria
            .into_iter()
            .filter(|criterion| !criterion_met(criterion, &decided.answers))
            .collect();
        let approved = failed.is_empty();
        let feedback = if approved {
            String::new()
        } else {
            compose_refusal(&failed)
        };

        let mut active: ActiveModel = tool.into();
        active.status = Set(if approved {
            Status::Validated
        } else {
            Status::Draft
        });
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;

        Ok((approved, feedback))
    }

    pub async fn activate_tool(&self, tool_id: i64, user_id: Uuid) -> Result<(), ToolError> {
        let tool = self.user_owned_tool(tool_id, user_id).await?;
        if tool.status != Status::Validated {
            return Err(ToolError::InvalidRequest(format!(
                "tool {tool_id} is not Validated"
            )));
        }
        let mut active: ActiveModel = tool.into();
        active.status = Set(Status::Active);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn list_tools(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<Vec<crate::entity::tool::Model>, ToolError> {
        let owner_filter = user_id.map_or_else(
            || crate::entity::tool::Column::UserId.is_null(),
            |id| {
                crate::entity::tool::Column::UserId
                    .is_null()
                    .or(crate::entity::tool::Column::UserId.eq(id))
            },
        );
        Ok(Entity::find()
            .filter(owner_filter)
            .filter(crate::entity::tool::Column::Status.eq(Status::Active))
            .all(&self.db)
            .await?)
    }

    async fn user_owned_tool(
        &self,
        tool_id: i64,
        user_id: Uuid,
    ) -> Result<crate::entity::tool::Model, ToolError> {
        Entity::find_by_id(tool_id)
            .filter(crate::entity::tool::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
            .ok_or(ToolError::NotFound(tool_id))
    }

    pub async fn execute(
        &self,
        caller: Option<Uuid>,
        slug: &str,
        input_json: &str,
        idempotency_key: &str,
    ) -> Result<ExecuteOutcome, ToolError> {
        tracing::debug!(
            idempotency_key,
            slug,
            "tool execute (key logged, not enforced — no destructive side effect here yet, see the spec's Порты-equivalent note)"
        );
        let Some(tool) = self.find_tool(caller, slug).await? else {
            return Ok(ExecuteOutcome::Error(format!("no tool with slug {slug:?}")));
        };
        if crate::policy::check_policy(tool.risk) == crate::policy::PolicyDecision::RequiresApproval
        {
            return Ok(ExecuteOutcome::RequiresApproval);
        }
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
        let timeout = registered_timeout(tool.timeout_seconds);
        if let Some(raw) = tool.sources.clone() {
            return self.run_declarative(&tool, &raw, &input, timeout).await;
        }
        self.run_system_tool(slug, &input, timeout).await
    }

    /// A row that declares its sources runs through one shared executor: its slug is a name, not a
    /// branch, and this service holds no knowledge of what any of them is about.
    async fn run_declarative(
        &self,
        tool: &crate::entity::tool::Model,
        raw: &Value,
        input: &Value,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        let set = match crate::tools::declarative::parse_sources(raw, &tool.input_schema) {
            Ok(set) => set,
            Err(problem) => return Ok(ExecuteOutcome::NotExecutable(problem)),
        };
        Ok(
            match crate::tools::declarative::run(
                &self.http,
                &set,
                input,
                timeout,
                |url| async move { crate::tools::web_fetch::ensure_public_url(&url).await },
            )
            .await
            {
                Ok(value) => ExecuteOutcome::Ok(value),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }

    async fn find_tool(
        &self,
        caller: Option<Uuid>,
        slug: &str,
    ) -> Result<Option<crate::entity::tool::Model>, ToolError> {
        if let Some(user_id) = caller {
            let own = Entity::find()
                .filter(crate::entity::tool::Column::UserId.eq(user_id))
                .filter(crate::entity::tool::Column::Slug.eq(slug))
                .one(&self.db)
                .await?;
            if own.is_some() {
                return Ok(own);
            }
        }
        Ok(Entity::find()
            .filter(crate::entity::tool::Column::UserId.is_null())
            .filter(crate::entity::tool::Column::Slug.eq(slug))
            .one(&self.db)
            .await?)
    }

    async fn run_system_tool(
        &self,
        slug: &str,
        input: &Value,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        match slug {
            crate::slugs::WEB_SEARCH => match crate::args::parse::<crate::args::Search>(input) {
                Ok(args) => self.run_web_search(args).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::WEB_FETCH => match crate::args::parse::<crate::args::Fetch>(input) {
                Ok(args) => self.run_web_fetch(args, timeout).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::KB_SEARCH => match crate::args::parse::<crate::args::Search>(input) {
                Ok(args) => self.run_kb_search(args).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::KB_READ_DOCUMENT => {
                match crate::args::parse::<crate::args::ReadDocument>(input) {
                    Ok(args) => self.run_kb_read_document(args).await,
                    Err(problem) => Ok(ExecuteOutcome::Error(problem)),
                }
            }
            other => Ok(ExecuteOutcome::NotExecutable(format!(
                "tool {other:?} has no runnable implementation yet — user-created tools need Integrations Service"
            ))),
        }
    }

    async fn run_web_search(&self, args: crate::args::Search) -> Result<ExecuteOutcome, ToolError> {
        let limit = match args.checked_limit() {
            Ok(limit) => limit,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        let mut results = match self.search.search(&args.query, limit).await {
            Ok(results) => results,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        // See docs/superpowers/plans/2026-09-18-latency-round-two.md, Task C: attaching the
        // readable text of the top hits here removes the round a model otherwise spends asking
        // for the same URLs back by name. This can only add a `text` field, never fail the
        // search — see `attach_prefetched_text`'s own doc comment.
        crate::tools::web_search::attach_prefetched_text(&self.http, &mut results).await;
        Ok(ExecuteOutcome::Ok(serde_json::to_value(results)?))
    }

    async fn run_web_fetch(
        &self,
        args: crate::args::Fetch,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        let urls = match args.checked_urls() {
            Ok(urls) => urls,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        if let Err(error) = ensure_all_urls_public(&urls).await {
            return Ok(ExecuteOutcome::Error(error));
        }
        Ok(
            match crate::tools::web_fetch::fetch_richest(&self.http, &urls, timeout).await {
                Ok(page) => ExecuteOutcome::Ok(crate::tools::web_fetch::readable_page_json(&page)),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }

    async fn run_kb_search(&self, args: crate::args::Search) -> Result<ExecuteOutcome, ToolError> {
        let limit = match args.checked_limit() {
            Ok(limit) => limit,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        Ok(match self.kb.search(&args.query, limit).await {
            Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
            Err(error) => ExecuteOutcome::Error(error),
        })
    }

    async fn run_kb_read_document(
        &self,
        args: crate::args::ReadDocument,
    ) -> Result<ExecuteOutcome, ToolError> {
        let version = args.version.unwrap_or_default();
        Ok(
            match self
                .kb
                .read_document(&args.source, &args.source_id, &version)
                .await
            {
                Ok(content) => ExecuteOutcome::Ok(serde_json::json!({"content": content})),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }
}

fn claims_reserved_slug(user_id: Option<Uuid>, slug: &str) -> bool {
    user_id.is_some() && crate::slugs::RESERVED_FOR_SYSTEM_TOOLS.contains(&slug)
}

fn already_reviewed(status: Status) -> bool {
    status != Status::Draft
}

fn criterion_question(criterion: &Criterion) -> Result<Question, ToolError> {
    let mut question: Question = serde_json::from_value(serde_json::json!({
        "instructions": criterion.instructions,
    }))
    .map_err(|e| ToolError::InvalidRequest(format!("could not build the question: {e}")))?;
    question.id = criterion.id.to_owned();
    question.kind = Noul {
        when_true: Some(criterion.when_true.to_owned()),
        when_false: Some(criterion.when_false.to_owned()),
        ..Default::default()
    }
    .into();
    Ok(question)
}

// Looks the answer up by the exact id this criterion asked under, and only accepts a `noul`. A
// missing id, a foreign id, or a `choice`/`score` answer under the right id all fall through to
// `false` here, not to a separate error path — see `validate_tool`'s fail-closed comment.
fn criterion_met(criterion: &Criterion, answers: &[Answer]) -> bool {
    answers
        .iter()
        .find(|answer| answer.id == criterion.id)
        .and_then(|answer| match answer.answer.as_ref() {
            Some(Given::Noul(noul)) => Some(noul.noul),
            _ => None,
        })
        .is_some_and(|noul| noul >= APPROVAL_THRESHOLD)
}

// The refusal is assembled from whichever criteria failed, in Rust, with no generative call: each
// criterion's `when_false` was already written to read as a reason, so composing them is just
// naming which ones applied.
fn compose_refusal(failed: &[Criterion]) -> String {
    let reasons: String = failed
        .iter()
        .map(|criterion| format!("- {}: {}", criterion.id, criterion.when_false))
        .collect::<Vec<_>>()
        .join("\n");
    format!("This tool definition does not meet the requirements:\n{reasons}")
}

/// Tells apart a `Decide` failure that is the submitter's to fix from one that is not. llm-router
/// already separates the two at the wire: `invalid_argument` is a request its caller (this
/// service) sent that the submitter's own input made too large or otherwise malformed — the state
/// budget is the live example, since a huge description makes a huge state. Every other code
/// (`unavailable`, `internal`, `failed_precondition`, a deadline this client's own `VALIDATE_TIMEOUT`
/// hit, ...) is a transient or deployment-side fault that has nothing to do with what the submitter
/// wrote, so it must not be reported as an invalid request — see `ToolError::ReviewUnavailable`.
/// Both variants fail closed: neither one is ever turned into an approval.
fn decide_call_error(error: ConnectError) -> ToolError {
    if error.code == ErrorCode::InvalidArgument {
        ToolError::InvalidRequest(format!(
            "the tool definition could not be reviewed: {error}"
        ))
    } else {
        ToolError::ReviewUnavailable(format!("tool review is temporarily unavailable: {error}"))
    }
}

// What the decider is told about the tool under review.
fn definition_text(tool: &crate::entity::tool::Model) -> String {
    format!(
        "name: {}\ndescription: {}\ninput_schema: {}\noutput_schema: {}\nrisk: {:?}\ntimeout_seconds: {}",
        tool.name,
        tool.description,
        tool.input_schema,
        tool.output_schema,
        tool.risk,
        tool.timeout_seconds,
    )
}

async fn ensure_all_urls_public(urls: &[String]) -> Result<(), String> {
    for url in urls {
        crate::tools::web_fetch::ensure_public_url(url).await?;
    }
    Ok(())
}

fn registered_timeout(timeout_seconds: i32) -> Duration {
    Duration::from_secs(
        timeout_seconds
            .clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS)
            .unsigned_abs()
            .into(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecuteOutcome {
    Ok(Value),
    RequiresApproval,
    NotExecutable(String),
    Error(String),
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::llm_router::v1::{
        Answer, ChoiceAnswer, CompleteRequest, CompleteResponse, DecideResponse,
        DescribeModelsRequest, DescribeModelsResponse, DescribeTiersRequest, DescribeTiersResponse,
        LlmRouterService, NoulAnswer, SystemOneService,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_TIMEOUT_SECONDS: i32 = 30;
    // A fixed owner so validate_tool tests can share draft_tool without minting a Uuid each time.
    const OWNER: Uuid = Uuid::from_u128(1);

    /// A local mirror of `services/llm-router`'s `adapters::system_one::REQUEST_TIMEOUT` — this
    /// crate has no dependency on llm-router to check against the real constant, so this pins the
    /// value `VALIDATE_TIMEOUT` is kept strictly above by hand, the same way llm-router's own
    /// adapter tests mirror Chat's `DECIDE_CALL_TIMEOUT`. Keep in sync with the real constant.
    const SYSTEM_ONE_REQUEST_TIMEOUT_MIRROR: Duration = Duration::from_millis(1500);

    // The same race the whole-branch review traced for Chat's matching pair: an outer deadline
    // that is not strictly above llm-router's inner one always wins it, silently dropping the
    // audit row for the slow or failed attempt this timeout exists to bound. See
    // `VALIDATE_TIMEOUT`'s doc comment. This bound is the only hard requirement on the value —
    // `VALIDATE_TIMEOUT` sits at 10 s, well above Chat's and the engine's matching 2 s constants,
    // because this caller has no fallback if `Decide` fails and one human is waiting on one
    // submission, not a live conversation.
    #[test]
    fn validate_timeout_stays_strictly_above_the_adapters_inner_deadline() {
        assert!(
            VALIDATE_TIMEOUT > SYSTEM_ONE_REQUEST_TIMEOUT_MIRROR,
            "{VALIDATE_TIMEOUT:?} vs {SYSTEM_ONE_REQUEST_TIMEOUT_MIRROR:?}"
        );
    }

    fn a_tool(slug: &str, name: &str, description: &str, risk: Risk) -> NewTool {
        NewTool::checked(
            slug.to_owned(),
            name.to_owned(),
            description.to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            risk,
            TEST_TIMEOUT_SECONDS,
        )
        .expect("a valid tool definition")
    }

    async fn draft_tool(service: &Service) -> i64 {
        service
            .create_tool(
                Some(OWNER),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create")
    }

    struct FakeLlmRouter {
        completions: Arc<AtomicUsize>,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            // validate_tool no longer holds an LlmRouterServiceClient at all, so nothing in this
            // service can reach this arm — it exists only so a test can prove that by asserting
            // `completions` stays at 0.
            self.completions.fetch_add(1, Ordering::SeqCst);
            Response::ok(CompleteResponse::default())
        }
        async fn describe_tiers(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeTiersRequest>,
        ) -> ServiceResult<DescribeTiersResponse> {
            Response::ok(DescribeTiersResponse::default())
        }
    }

    /// `Echo(noul)` answers every question a request actually asks, under the id it was asked
    /// under, with the same score — the "everything passes" / "everything fails" fake most tests
    /// want. `Fixed(answers)` ignores what was asked and returns exactly that list instead, for
    /// exercising the fail-closed paths where the decision does not line up with the questions:
    /// no answers at all, an answer under an id never asked, or a `choice`/`score` where a `noul`
    /// was asked.
    enum FakeSystemOne {
        Echo(f64),
        Fixed(Vec<Answer>),
    }

    #[allow(refining_impl_trait)]
    impl SystemOneService for FakeSystemOne {
        async fn decide(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, DecideRequest>,
        ) -> ServiceResult<DecideResponse> {
            let answers = match self {
                FakeSystemOne::Echo(noul) => request
                    .to_owned_message()
                    .questions
                    .into_iter()
                    .map(|question| noul_answer(&question.id, *noul))
                    .collect(),
                FakeSystemOne::Fixed(answers) => answers.clone(),
            };
            Response::ok(DecideResponse {
                answers,
                ..Default::default()
            })
        }

        async fn describe_models(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeModelsRequest>,
        ) -> ServiceResult<DescribeModelsResponse> {
            Response::ok(DescribeModelsResponse::default())
        }
    }

    fn noul_answer(id: &str, noul: f64) -> Answer {
        Answer {
            id: id.to_owned(),
            answer: NoulAnswer {
                noul,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    fn choice_answer(id: &str, choice: &str) -> Answer {
        Answer {
            id: id.to_owned(),
            answer: ChoiceAnswer {
                choice: choice.to_owned(),
                confidence: 0.9,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    // The same URL now serves both LlmRouterService and SystemOneService, so the fake mounts both
    // on one router. `completions` counts Complete calls, so a test can prove a path made none.
    async fn serve_llm(decider: FakeSystemOne) -> (String, Arc<AtomicUsize>) {
        let completions = Arc::new(AtomicUsize::new(0));
        let llm = Arc::new(FakeLlmRouter {
            completions: completions.clone(),
        });
        let decider = Arc::new(decider);
        let connect = ConnectRouter::new().add_service(llm).add_service(decider);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://{address}"), completions)
    }

    async fn serve_llm_counting_decisions(noul: f64) -> (String, Arc<AtomicUsize>) {
        serve_llm(FakeSystemOne::Echo(noul)).await
    }

    async fn serve_llm_with_answers(answers: Vec<Answer>) -> (String, Arc<AtomicUsize>) {
        serve_llm(FakeSystemOne::Fixed(answers)).await
    }

    async fn service_with(llm_url: &str) -> (crate::test_db::TestDb, Service) {
        full_service_with(llm_url, "http://127.0.0.1:1").await
    }

    async fn full_service_with(llm_url: &str, kb_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(
            test.db.clone(),
            llm_url,
            "unused-in-these-tests".to_owned(),
            "unused-in-these-tests".to_owned(),
            kb_url,
        )
        .expect("service");
        (test, service)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_confident_yes_validates_without_asking_for_words() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(approved);
        assert!(
            feedback.is_empty(),
            "an approval needs no prose: {feedback:?}"
        );
        assert_eq!(
            completions.load(Ordering::SeqCst),
            0,
            "Complete was called on the happy path"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_names_the_failed_criterion_and_makes_no_completions() {
        let answers = vec![
            noul_answer("input_schema", 0.1),
            noul_answer("output_schema", 0.9),
            noul_answer("description_purpose", 0.9),
            noul_answer("timeout", 0.9),
        ];
        let (llm_url, completions) = serve_llm_with_answers(answers).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved);
        assert!(
            feedback.contains("input_schema"),
            "the refusal must name the criterion that failed: {feedback}"
        );
        assert!(
            !feedback.contains("output_schema") && !feedback.contains("description_purpose"),
            "a refusal must not name a criterion that passed: {feedback}"
        );
        assert_eq!(
            completions.load(Ordering::SeqCst),
            0,
            "a refusal is composed in Rust, not written by a second model"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_decision_with_no_answers_refuses_naming_every_criterion() {
        let (llm_url, completions) = serve_llm_with_answers(Vec::new()).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved, "an empty decision must never approve");
        for id in [
            "input_schema",
            "output_schema",
            "description_purpose",
            "timeout",
        ] {
            assert!(feedback.contains(id), "{id} missing from {feedback}");
        }
        assert_eq!(completions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_under_an_id_never_asked_does_not_approve_that_criterion() {
        let answers = vec![
            noul_answer("input_schema", 0.9),
            noul_answer("output_schema", 0.9),
            noul_answer("description_purpose", 0.9),
            // A confident answer, but under the wrong id: "timeout" itself is left unanswered.
            noul_answer("timeout_seconds", 0.99),
        ];
        let (llm_url, completions) = serve_llm_with_answers(answers).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(
            !approved,
            "a high-confidence answer under a foreign id must not approve anything"
        );
        assert!(feedback.contains("timeout"), "{feedback}");
        assert_eq!(completions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_choice_answer_where_a_noul_was_asked_does_not_approve() {
        let answers = vec![
            noul_answer("input_schema", 0.9),
            noul_answer("output_schema", 0.9),
            noul_answer("description_purpose", 0.9),
            choice_answer("timeout", "plausible"),
        ];
        let (llm_url, completions) = serve_llm_with_answers(answers).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(
            !approved,
            "a choice answer must not satisfy a noul question"
        );
        assert!(feedback.contains("timeout"), "{feedback}");
        assert_eq!(completions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_tool_must_state_what_it_changes() {
        let answers = vec![
            noul_answer("input_schema", 0.9),
            noul_answer("output_schema", 0.9),
            noul_answer("description_purpose", 0.9),
            noul_answer("timeout", 0.9),
            noul_answer("description_changes", 0.1),
        ];
        let (llm_url, completions) = serve_llm_with_answers(answers).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = service
            .create_tool(
                Some(OWNER),
                a_tool("send_email", "Send Email", "Sends an email", Risk::Write),
            )
            .await
            .expect("create");

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved);
        assert!(feedback.contains("description_changes"), "{feedback}");
        assert_eq!(completions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_tool_that_states_what_it_changes_is_approved() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = service
            .create_tool(
                Some(OWNER),
                a_tool(
                    "send_email",
                    "Send Email",
                    "Sends an email and records the delivery in the audit log",
                    Risk::Write,
                ),
            )
            .await
            .expect("create");

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(approved, "{feedback}");
        assert_eq!(completions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_decider_reports_review_unavailable_and_does_not_approve() {
        let (_test, service) = service_with("http://127.0.0.1:1").await;
        let tool_id = draft_tool(&service).await;

        let error = service.validate_tool(tool_id, OWNER).await.unwrap_err();

        assert!(
            matches!(error, ToolError::ReviewUnavailable(_)),
            "an unreachable decider is not the submitter's fault to fix: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_rejects_a_user_owned_system_slug() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();

        let error = service
            .create_tool(
                Some(user_id),
                a_tool(
                    crate::slugs::WEB_SEARCH,
                    "Shadow web search",
                    "d",
                    Risk::ReadOnly,
                ),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_starts_as_draft() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();

        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let row = crate::entity::tool::Entity::find_by_id(tool_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, Status::Draft);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_approves_and_advances_to_validated() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(approved);
        assert!(feedback.is_empty());
        let row = crate::entity::tool::Entity::find_by_id(tool_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, Status::Validated);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejects_a_tool_that_is_no_longer_draft() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;
        service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("first validate");

        let error = service.validate_tool(tool_id, OWNER).await.unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejection_stays_draft() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.08).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, _) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_tool_requires_validated_status() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        assert!(
            service.activate_tool(tool_id, OWNER).await.is_err(),
            "still Draft, not Validated"
        );

        service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, OWNER)
            .await
            .expect("activate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_tools_hides_other_users_tools() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        create_listable_tool(&service, owner, a_tool("mine", "Mine", "d", Risk::ReadOnly)).await;

        let seen_by_stranger = service.list_tools(Some(stranger)).await.expect("list");
        let seen_by_owner = service.list_tools(Some(owner)).await.expect("list");

        assert!(seen_by_stranger.iter().all(|t| t.slug != "mine"));
        assert!(seen_by_owner.iter().any(|t| t.slug == "mine"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_read_only_system_tool_runs_it() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        seed_system_tools(&service).await;

        let outcome = service
            .execute(
                None,
                crate::slugs::KB_SEARCH,
                r#"{"query": "borrow checker"}"#,
                "e:kb_search:0",
            )
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Ok(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_argument_the_tool_does_not_declare_is_refused_naming_the_one_it_takes() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        seed_system_tools(&service).await;

        let outcome = service
            .execute(
                None,
                crate::slugs::KB_SEARCH,
                r#"{"q": "borrow checker"}"#,
                "e:kb_search:0",
            )
            .await
            .expect("execute");

        let ExecuteOutcome::Error(message) = outcome else {
            panic!("a misspelled argument must not reach the provider");
        };
        assert!(message.contains('q'), "{message}");
        assert!(message.contains("query"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_an_unregistered_slug_is_an_error() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;

        let outcome = service
            .execute(None, "nonexistent", "{}", "e:x:0")
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Error(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_write_tool_requires_approval_without_running_it() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                a_tool("send_email", "Send Email", "Sends an email", Risk::Write),
            )
            .await
            .expect("create");

        let outcome = service
            .execute(Some(user_id), "send_email", "{}", "e:mail:0")
            .await
            .expect("execute");

        assert_eq!(outcome, ExecuteOutcome::RequiresApproval);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_user_created_tool_is_not_executable() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                a_tool("custom_thing", "Custom", "d", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let outcome = service
            .execute(Some(user_id), "custom_thing", "{}", "e:c:0")
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::NotExecutable(_)));
    }

    #[test]
    fn a_name_longer_than_the_column_is_an_invalid_argument_not_a_db_error() {
        let error = NewTool::checked(
            "echo".to_owned(),
            "e".repeat(MAX_NAME_CHARS + 1),
            "Echoes input".to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            Risk::ReadOnly,
            TEST_TIMEOUT_SECONDS,
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)), "{error}");
    }

    #[test]
    fn a_slug_longer_than_the_column_is_an_invalid_argument() {
        let error = NewTool::checked(
            "s".repeat(MAX_SLUG_CHARS + 1),
            "Echo".to_owned(),
            "Echoes input".to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            Risk::ReadOnly,
            TEST_TIMEOUT_SECONDS,
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)), "{error}");
    }

    #[test]
    fn a_timeout_outside_the_validated_range_is_rejected_at_entry() {
        for timeout_seconds in [MIN_TIMEOUT_SECONDS - 1, MAX_TIMEOUT_SECONDS + 1] {
            let error = NewTool::checked(
                "echo".to_owned(),
                "Echo".to_owned(),
                "Echoes input".to_owned(),
                json!({"type": "object"}),
                json!({"type": "object"}),
                Risk::ReadOnly,
                timeout_seconds,
            )
            .unwrap_err();

            assert!(
                matches!(error, ToolError::InvalidRequest(_)),
                "{timeout_seconds}"
            );
        }
    }

    async fn create_listable_tool(service: &Service, owner: Uuid, tool: NewTool) {
        let tool_id = service
            .create_tool(Some(owner), tool)
            .await
            .expect("create");
        service
            .validate_tool(tool_id, owner)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, owner)
            .await
            .expect("activate");
    }

    async fn seed_system_tools(service: &Service) {
        let mut kb_search = a_tool(
            crate::slugs::KB_SEARCH,
            "Knowledge base search",
            "Searches the knowledge base",
            Risk::ReadOnly,
        );
        kb_search.input_schema = crate::args::input_schema::<crate::args::Search>();
        service
            .create_tool(None, kb_search)
            .await
            .expect("seed kb_search");
    }
}
