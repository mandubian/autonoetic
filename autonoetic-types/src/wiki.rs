use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPageEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiListResult {
    pub pages: Vec<WikiPageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiGetParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiGetResult {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
}
