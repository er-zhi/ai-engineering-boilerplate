// Runs every golden case against the live stack through Gateway and writes down what happened, for a model to judge afterwards against `cases.json`'s written expectations.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_GATEWAY_URL: &str = "http://localhost:8080";
const CASE_DEADLINE: Duration = Duration::from_secs(60);
const TOPIC_POLL_INTERVAL: Duration = Duration::from_millis(120);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct Case {
    id: String,
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    messages: Vec<String>,
    #[serde(default)]
    query: String,
    expect: String,
}

#[derive(Serialize)]
struct Outcome {
    id: String,
    kind: String,
    asked: String,
    expect: String,
    elapsed_ms: u128,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topics: Vec<TopicOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    turns: Vec<TurnOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    results: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct TurnOutcome {
    said: String,
    topics: Vec<TopicOutcome>,
}

#[derive(Serialize)]
struct TopicOutcome {
    title: String,
    question: String,
    status: String,
    answer: String,
    finished_at_ms: u128,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("GOLDEN_BASE_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_owned());
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
    let said = case.said();
    let asked = if case.kind == "kb" {
        case.query.clone()
    } else {
        said.join(" → ")
    };
    let mut outcome = Outcome {
        id: case.id.clone(),
        kind: case.kind.clone(),
        asked,
        expect: case.expect.clone(),
        elapsed_ms: 0,
        topics: Vec::new(),
        turns: Vec::new(),
        results: Vec::new(),
        error: None,
    };
    let ran = match case.kind.as_str() {
        "kb" => search_knowledge_base(client, base, &case.query)
            .await
            .map(|results| outcome.results = results),
        "chat" => replay_a_conversation(client, base, &said)
            .await
            .map(|turns| {
                // One message stays one topic list, so a judge reads the common case unchanged.
                if turns.len() == 1 {
                    outcome.topics = turns
                        .into_iter()
                        .next()
                        .unwrap_or(TurnOutcome {
                            said: String::new(),
                            topics: Vec::new(),
                        })
                        .topics;
                } else {
                    outcome.turns = turns;
                }
            }),
        other => Err(format!("unknown case kind {other}").into()),
    };
    if let Err(error) = ran {
        outcome.error = Some(error.to_string());
    }
    outcome.elapsed_ms = started.elapsed().as_millis();
    outcome
}

impl Case {
    fn said(&self) -> Vec<String> {
        if self.messages.is_empty() {
            vec![self.message.clone()]
        } else {
            self.messages.clone()
        }
    }
}

async fn replay_a_conversation(
    client: &reqwest::Client,
    base: &str,
    said: &[String],
) -> Result<Vec<TurnOutcome>, Box<dyn std::error::Error>> {
    rpc(client, base, "chat.v1.ChatService/ResetSession", &json!({})).await?;
    let mut turns = Vec::new();
    for message in said {
        turns.push(TurnOutcome {
            said: message.clone(),
            topics: send_turn_and_wait(client, base, message).await?,
        });
    }
    Ok(turns)
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

async fn send_turn_and_wait(
    client: &reqwest::Client,
    base: &str,
    message: &str,
) -> Result<Vec<TopicOutcome>, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let sent = rpc(
        client,
        base,
        "chat.v1.ChatService/SendTurn",
        &json!({"turnId": uuid::Uuid::new_v4().to_string(), "content": message}),
    )
    .await?;

    let topics_this_turn_opened: Vec<String> = sent["topicIds"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let nothing_to_wait_for = topics_this_turn_opened.is_empty();
    if nothing_to_wait_for {
        return Ok(Vec::new());
    }
    wait_until_every_topic_settles(client, base, &topics_this_turn_opened, started).await
}

async fn wait_until_every_topic_settles(
    client: &reqwest::Client,
    base: &str,
    opened: &[String],
    started: Instant,
) -> Result<Vec<TopicOutcome>, Box<dyn std::error::Error>> {
    let mut finished_at: BTreeMap<String, u128> = BTreeMap::new();
    loop {
        let session = rpc(client, base, "chat.v1.ChatService/GetSession", &json!({})).await?;
        let topics = topics_among(&session, opened);
        let settled = topics.len() == opened.len()
            && topics
                .iter()
                .all(|topic| note_if_settled(topic, started, &mut finished_at));
        if settled {
            return Ok(described(&topics, &finished_at));
        }
        if started.elapsed() > CASE_DEADLINE {
            return Err("the turn did not finish within the deadline".into());
        }
        tokio::time::sleep(TOPIC_POLL_INTERVAL).await;
    }
}

fn topics_among(session: &Value, opened: &[String]) -> Vec<Value> {
    session["topics"]
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
        .unwrap_or_default()
}

fn note_if_settled(
    topic: &Value,
    started: Instant,
    finished_at: &mut BTreeMap<String, u128>,
) -> bool {
    let status = topic["status"].as_str().unwrap_or_default();
    // Anything this harness does not recognise counts as still running, never as done: a status
    // added to the contract must not make every case complete instantly with an empty answer.
    let settled = matches!(
        status,
        "TOPIC_STATUS_COMPLETED" | "TOPIC_STATUS_FAILED" | "TOPIC_STATUS_CANCELLED"
    );
    if settled {
        finished_at
            .entry(topic["id"].as_str().unwrap_or_default().to_owned())
            .or_insert_with(|| started.elapsed().as_millis());
    }
    settled
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
