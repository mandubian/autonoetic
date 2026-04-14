//! Mock Moltbook Server
//!
//! A minimal HTTP server that simulates the Moltbook external service API for
//! testing the agent registration workflow with human intervention.
//!
//! The workflow this mock drives:
//!   1. Agent POSTs /api/register-agent → gets agent_id + secret
//!   2. Agent POSTs /api/human-claim    → gets verification_tweet_text
//!   3. Human posts that tweet and gives the URL back to the agent
//!   4. Agent POSTs /api/verify-human-claim → marks agent as verified
//!   5. Agent POSTs /api/setup-heartbeat → enables periodic activity
//!   6. Agent can now POST /api/post-to-feed
//!
//! Usage:
//!   cargo run --bin mock-moltbook
//!
//! The server listens on 0.0.0.0:8765 by default.
//! Override with MOCK_MOLTBOOK_PORT environment variable.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// In-memory state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    agents: Arc<Mutex<HashMap<String, Agent>>>,
    verifications: Arc<Mutex<HashMap<String, VerificationEntry>>>,
    posts: Arc<Mutex<Vec<Post>>>,
}

#[derive(Clone)]
struct Agent {
    id: String,
    name: String,
    model: String,
    secret: String,
    human_x_username: Option<String>,
    verified: bool,
    created_at: String,
}

#[derive(Clone)]
struct VerificationEntry {
    #[allow(dead_code)]
    agent_id: String,
    #[allow(dead_code)]
    tweet_text: String,
    tweet_url: Option<String>,
    verified: bool,
}

#[derive(Serialize, Clone)]
struct Post {
    id: String,
    agent_id: String,
    content: String,
    timestamp: String,
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterAgentReq {
    name: String,
    model: String,
}

#[derive(Serialize)]
struct RegisterAgentResp {
    agent_id: String,
    secret: String,
    message: String,
}

#[derive(Deserialize)]
struct HumanClaimReq {
    human_x_username: String,
}

#[derive(Serialize)]
struct HumanClaimResp {
    verification_tweet_text: String,
    message: String,
}

#[derive(Deserialize)]
struct VerifyHumanClaimReq {
    tweet_url: String,
}

#[derive(Serialize)]
struct VerifyHumanClaimResp {
    success: bool,
    message: String,
}

#[derive(Deserialize)]
struct SetupHeartbeatReq {
    prompt_id: String,
    interval_hours: u32,
}

#[derive(Serialize)]
struct SetupHeartbeatResp {
    success: bool,
    message: String,
}

#[derive(Deserialize)]
struct PostToFeedReq {
    content: String,
}

#[derive(Serialize)]
struct PostToFeedResp {
    success: bool,
    post_id: String,
    message: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

fn find_agent_by_secret(agents: &HashMap<String, Agent>, secret: &str) -> Option<String> {
    agents
        .values()
        .find(|a| a.secret == secret)
        .map(|a| a.id.clone())
}

fn short_uuid() -> String {
    Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
    "Mock Moltbook is running\n"
}

async fn skill_md() -> (StatusCode, HeaderMap, String) {
    let content = include_str!("mock_moltbook_skill.md").to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/markdown; charset=utf-8".parse().unwrap(),
    );
    println!("[mock-moltbook] GET /skill.md");
    (StatusCode::OK, headers, content)
}

async fn register_agent(
    State(state): State<MockState>,
    Json(req): Json<RegisterAgentReq>,
) -> Result<Json<RegisterAgentResp>, StatusCode> {
    let agent_id = format!("moltbook_agent_{}", short_uuid());
    let secret = format!("sk_molt_{}", Uuid::new_v4().to_string().replace('-', ""));
    let now = chrono::Utc::now().to_rfc3339();

    let agent = Agent {
        id: agent_id.clone(),
        name: req.name.clone(),
        model: req.model.clone(),
        secret: secret.clone(),
        human_x_username: None,
        verified: false,
        created_at: now,
    };

    state.agents.lock().unwrap().insert(agent_id.clone(), agent);

    println!(
        "[mock-moltbook] REGISTER  name={} model={} agent_id={} secret={}",
        req.name, req.model, agent_id, secret
    );

    Ok(Json(RegisterAgentResp {
        agent_id,
        secret,
        message: "Agent registered. Store the secret securely — it cannot be recovered."
            .to_string(),
    }))
}

async fn human_claim(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(req): Json<HumanClaimReq>,
) -> Result<Json<HumanClaimResp>, StatusCode> {
    let secret = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let agents = state.agents.lock().unwrap();
    let agent_id = find_agent_by_secret(&agents, &secret).ok_or(StatusCode::UNAUTHORIZED)?;
    drop(agents);

    let code = short_uuid().to_uppercase();
    let tweet_text = format!(
        "I am verifying my AI agent {} on Moltbook. Verification code: #MoltbookVerify{}",
        agent_id, code
    );

    state.verifications.lock().unwrap().insert(
        agent_id.clone(),
        VerificationEntry {
            agent_id: agent_id.clone(),
            tweet_text: tweet_text.clone(),
            tweet_url: None,
            verified: false,
        },
    );

    if let Some(agent) = state.agents.lock().unwrap().get_mut(&agent_id) {
        agent.human_x_username = Some(req.human_x_username.clone());
    }

    println!(
        "[mock-moltbook] HUMAN_CLAIM  agent_id={} username={} tweet_text={}",
        agent_id, req.human_x_username, tweet_text
    );

    Ok(Json(HumanClaimResp {
        verification_tweet_text: tweet_text,
        message: "Post this tweet from your X account, then call /api/verify-human-claim with the tweet URL.".to_string(),
    }))
}

async fn verify_human_claim(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(req): Json<VerifyHumanClaimReq>,
) -> Result<Json<VerifyHumanClaimResp>, StatusCode> {
    let secret = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let agents = state.agents.lock().unwrap();
    let agent_id = find_agent_by_secret(&agents, &secret).ok_or(StatusCode::UNAUTHORIZED)?;
    drop(agents);

    if let Some(v) = state.verifications.lock().unwrap().get_mut(&agent_id) {
        v.tweet_url = Some(req.tweet_url.clone());
        v.verified = true;
    }

    if let Some(agent) = state.agents.lock().unwrap().get_mut(&agent_id) {
        agent.verified = true;
    }

    println!(
        "[mock-moltbook] VERIFY  agent_id={} tweet_url={} status=VERIFIED",
        agent_id, req.tweet_url
    );

    Ok(Json(VerifyHumanClaimResp {
        success: true,
        message: "Human claim verified. Agent is now fully operational.".to_string(),
    }))
}

async fn setup_heartbeat(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(req): Json<SetupHeartbeatReq>,
) -> Result<Json<SetupHeartbeatResp>, StatusCode> {
    let secret = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let agents = state.agents.lock().unwrap();
    let agent = agents
        .values()
        .find(|a| a.secret == secret)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !agent.verified {
        return Ok(Json(SetupHeartbeatResp {
            success: false,
            message: "Agent must be verified before setting up heartbeat.".to_string(),
        }));
    }

    let agent_id = agent.id.clone();
    drop(agents);

    println!(
        "[mock-moltbook] HEARTBEAT  agent_id={} prompt_id={} interval_hours={}",
        agent_id, req.prompt_id, req.interval_hours
    );

    Ok(Json(SetupHeartbeatResp {
        success: true,
        message: format!(
            "Heartbeat configured. Will probe every {} hour(s).",
            req.interval_hours
        ),
    }))
}

async fn post_to_feed(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(req): Json<PostToFeedReq>,
) -> Result<Json<PostToFeedResp>, StatusCode> {
    let secret = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let agents = state.agents.lock().unwrap();
    let agent = agents
        .values()
        .find(|a| a.secret == secret)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !agent.verified {
        return Ok(Json(PostToFeedResp {
            success: false,
            post_id: String::new(),
            message: "Agent must be verified before posting to the feed.".to_string(),
        }));
    }

    let agent_id = agent.id.clone();
    drop(agents);

    let post_id = format!("post_{}", short_uuid());
    state.posts.lock().unwrap().push(Post {
        id: post_id.clone(),
        agent_id: agent_id.clone(),
        content: req.content.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    });

    println!(
        "[mock-moltbook] POST_TO_FEED  agent_id={} post_id={} content={}",
        agent_id, post_id, req.content
    );

    Ok(Json(PostToFeedResp {
        success: true,
        post_id,
        message: "Posted to feed successfully.".to_string(),
    }))
}

async fn server_status(State(state): State<MockState>) -> Json<serde_json::Value> {
    let agents = state.agents.lock().unwrap();
    let verifications = state.verifications.lock().unwrap();
    let posts = state.posts.lock().unwrap();

    let agent_list: Vec<serde_json::Value> = agents
        .values()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "name": a.name,
                "model": a.model,
                "verified": a.verified,
                "human_x_username": a.human_x_username,
                "created_at": a.created_at,
            })
        })
        .collect();

    Json(serde_json::json!({
        "total_agents": agents.len(),
        "verified_agents": agents.values().filter(|a| a.verified).count(),
        "pending_verifications": verifications.values().filter(|v| !v.verified).count(),
        "agents": agent_list,
        "posts": *posts,
    }))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("MOCK_MOLTBOOK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765);

    let state = MockState {
        agents: Arc::new(Mutex::new(HashMap::new())),
        verifications: Arc::new(Mutex::new(HashMap::new())),
        posts: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route("/", get(health))
        .route("/skill.md", get(skill_md))
        .route("/status", get(server_status))
        .route("/api/register-agent", post(register_agent))
        .route("/api/human-claim", post(human_claim))
        .route("/api/verify-human-claim", post(verify_human_claim))
        .route("/api/setup-heartbeat", post(setup_heartbeat))
        .route("/api/post-to-feed", post(post_to_feed))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Mock Moltbook server listening on http://{}", addr);
    println!("  GET  /              health check");
    println!("  GET  /skill.md      skill manifest");
    println!("  GET  /status        server state");
    println!("  POST /api/register-agent");
    println!("  POST /api/human-claim");
    println!("  POST /api/verify-human-claim");
    println!("  POST /api/setup-heartbeat");
    println!("  POST /api/post-to-feed");
    println!();

    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
