//! Runs every golden case against a live stack and writes down what happened. It decides nothing:
//! the answers are generated text, and comparing generated text to a stored string measures
//! phrasing rather than behaviour. So this records the facts — how many topics a message opened,
//! what each was titled and asked, what came back, and how long each took — and the judging is done
//! afterwards by a model reading the run against `cases.json`'s written expectations. See
//! `.agents/skills/golden/SKILL.md`.
//!
//! Needs the stack up (`docker compose up -d`) and `GATEWAY_AUTH_PASSWORD` in the environment, the
//! same one `.env` carries. Everything goes through Gateway, so this exercises exactly the path a
//! person's browser takes, authentication included.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where the stack is reachable. Gateway is the only published port, which is the point: a case
/// that passes here passed through the same door a person uses.
const DEFAULT_BASE: &str = "http://localhost:8080";
/// How long one case may take before it is recorded as unfinished. Generous on purpose — a case
/// that is merely slow is a finding, not a crash, and the judge should see the number.
const CASE_DEADLINE: Duration = Duration::from_secs(60);
/// How often a running turn is asked whether its topics have finished.
const POLL_INTERVAL: Duration = Duration::from_millis(120);
/// Long enough for a `Search` against a cold index, short enough that a hung one is obvious.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct Case {
    id: String,
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    query: String,
    expect: String,
}

/// One case's outcome, in the shape the judge reads. Nothing here is a verdict — `expect` travels
/// with the facts so the two can be read side by side.
#[derive(Serialize)]
struct Outcome {
    id: String,
    kind: String,
    asked: String,
    expect: String,
    /// Milliseconds from the turn being sent to the last topic finishing.
    elapsed_ms: u128,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topics: Vec<TopicOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    results: Vec<String>,
    /// Set when the case could not be run at all — the stack refused it, or it never finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct TopicOutcome {
    title: String,
    /// The self-contained question the topic was started with. A question still saying "there" is
    /// visible here and nowhere else.
    question: String,
    status: String,
    answer: String,
    /// Milliseconds from the turn being sent to this topic finishing, so a judge can see whether
    /// themes really did arrive independently.
    finished_at_ms: u128,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("GOLDEN_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_owned());
    let password = std::env::var("GATEWAY_AUTH_PASSWORD")
        .map_err(|_| "set GATEWAY_AUTH_PASSWORD (the same one .env carries)")?;
    let only = std::env::args().nth(1);

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(CALL_TIMEOUT)
        .build()?;
    log_in(&client, &base, &password).await?;

    let cases: Vec<Case> = serde_json::from_str(include_str!("../cases.json"))?;
    let mut outcomes = Vec::new();
    for case in cases {
        if only.as_ref().is_some_and(|wanted| &case.id != wanted) {
            continue;
        }
        eprintln!("· {}", case.id);
        outcomes.push(run_case(&client, &base, &case).await);
    }

    let run = json!({
        "ran_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default(),
        "base": base,
        "cases": outcomes,
    });
    println!("{}", serde_json::to_string_pretty(&run)?);
    Ok(())
}

async fn log_in(
    client: &reqwest::Client,
    base: &str,
    password: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = client
        .post(format!("{base}/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("password={}", urlencoded(password)))
        .send()
        .await?;
    if !response.status().is_success() && !response.status().is_redirection() {
        return Err(format!("could not log in: {}", response.status()).into());
    }
    Ok(())
}

/// Percent-encodes the few characters a password may carry that a form body reads as structure.
/// The runner has one form field and no need for a url-encoding crate to send it.
fn urlencoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn run_case(client: &reqwest::Client, base: &str, case: &Case) -> Outcome {
    let started = Instant::now();
    let asked = if case.kind == "kb" {
        case.query.clone()
    } else {
        case.message.clone()
    };
    let mut outcome = Outcome {
        id: case.id.clone(),
        kind: case.kind.clone(),
        asked,
        expect: case.expect.clone(),
        elapsed_ms: 0,
        topics: Vec::new(),
        results: Vec::new(),
        error: None,
    };
    let ran = match case.kind.as_str() {
        "kb" => search_knowledge_base(client, base, &case.query)
            .await
            .map(|results| outcome.results = results),
        "chat" => send_turn_and_wait(client, base, &case.message)
            .await
            .map(|topics| outcome.topics = topics),
        other => Err(format!("unknown case kind {other}").into()),
    };
    if let Err(error) = ran {
        outcome.error = Some(error.to_string());
    }
    outcome.elapsed_ms = started.elapsed().as_millis();
    outcome
}

async fn search_knowledge_base(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let body = rpc(
        client,
        base,
        "knowledge_base.v1.KnowledgeBaseService/Search",
        &json!({"query": query, "limit": 5}),
    )
    .await?;
    Ok(body["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|result| result["title"].as_str().unwrap_or("<untitled>").to_owned())
                .collect()
        })
        .unwrap_or_default())
}

/// Sends one turn into a session of its own and watches until every topic it opened has settled.
/// The session is reset first so no case can be answered out of another case's context.
async fn send_turn_and_wait(
    client: &reqwest::Client,
    base: &str,
    message: &str,
) -> Result<Vec<TopicOutcome>, Box<dyn std::error::Error>> {
    rpc(client, base, "chat.v1.ChatService/ResetSession", &json!({})).await?;
    let started = Instant::now();
    let sent = rpc(
        client,
        base,
        "chat.v1.ChatService/SendTurn",
        &json!({"turnId": uuid::Uuid::new_v4().to_string(), "content": message}),
    )
    .await?;

    // `SendTurn` names every topic the routing decision routed this turn to, so an empty list is a
    // finished answer — the turn carried no request — and there is nothing to wait for. Waiting out
    // the deadline instead would record a minute against a case that was settled in milliseconds.
    let opened: Vec<String> = sent["topicIds"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if opened.is_empty() {
        return Ok(Vec::new());
    }

    let mut finished_at: BTreeMap<String, u128> = BTreeMap::new();
    loop {
        let session = rpc(client, base, "chat.v1.ChatService/GetSession", &json!({})).await?;
        // Only the topics this turn opened. A session can hold others, and waiting on those would
        // attribute their time to this case.
        let topics: Vec<Value> = session["topics"]
            .as_array()
            .map(|topics| {
                topics
                    .iter()
                    .filter(|topic| {
                        topic["id"]
                            .as_str()
                            .is_some_and(|id| opened.iter().any(|wanted| wanted == id))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let settled = topics.len() == opened.len()
            && topics.iter().all(|topic| {
                let status = topic["status"].as_str().unwrap_or_default();
                let done = status != "TOPIC_STATUS_QUEUED" && status != "TOPIC_STATUS_RUNNING";
                if done {
                    finished_at
                        .entry(topic["id"].as_str().unwrap_or_default().to_owned())
                        .or_insert_with(|| started.elapsed().as_millis());
                }
                done
            });
        if settled {
            return Ok(described(&topics, &finished_at));
        }
        if started.elapsed() > CASE_DEADLINE {
            return Err("the turn did not finish within the deadline".into());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn described(topics: &[Value], finished_at: &BTreeMap<String, u128>) -> Vec<TopicOutcome> {
    topics
        .iter()
        .map(|topic| {
            let field = |name: &str| topic[name].as_str().unwrap_or_default().to_owned();
            TopicOutcome {
                title: field("title"),
                question: field("question"),
                status: field("status"),
                answer: field("resultSummary"),
                finished_at_ms: finished_at
                    .get(topic["id"].as_str().unwrap_or_default())
                    .copied()
                    .unwrap_or_default(),
            }
        })
        .collect()
}

async fn rpc(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    body: &Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = client
        .post(format!("{base}/{method}"))
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(format!("{method} answered {status}: {text}").into());
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({})))
}
