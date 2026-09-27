//! Session transcript helpers for the memory store.

use serde::{Deserialize, Serialize};

/// One turn in a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurn {
    pub seq: u64,
    pub role: SessionTurnRole,
    pub text: String,
    #[serde(default)] pub tags: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnRole {
    User,
    Assistant,
    System,
}

/// A session summary: short enough for the prompt, with enough for
/// `search_session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)] pub turns: Vec<SessionTurn>,
    #[serde(default)] pub last_fact: Option<String>,
    pub total_turns: usize,
}
