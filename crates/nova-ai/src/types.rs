use serde::{Deserialize, Serialize};
use std::{path::PathBuf, pin::Pin};

use futures::Stream;

use crate::AiError;

pub type AiStream = Pin<Box<dyn Stream<Item = Result<String, AiError>> + Send>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CodeSnippet {
    pub path: Option<PathBuf>,
    pub content: String,
}

impl CodeSnippet {
    pub fn new(path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
            content: content.into(),
        }
    }

    pub fn ad_hoc(content: impl Into<String>) -> Self {
        Self {
            path: None,
            content: content.into(),
        }
    }
}
