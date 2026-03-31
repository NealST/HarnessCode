//! Long-term memory storage — persists distilled knowledge cards across sessions.
//!
//! Cards live at `.harness/memories/mem-<id>.md` (human-readable markdown) and
//! an `index.json` tracks which sessions have been distilled and when.  The
//! format is designed to be git-friendly: one file per card, no binary blobs.

use crate::llm::{LlmMessage, LlmProvider};
use crate::memory::MemoryError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tracing::warn;

// ──────────────────────────────────────────────
// MemoryCard
// ──────────────────────────────────────────────

/// A distilled knowledge card extracted from a session.
///
/// Stored as a markdown file so it is human-readable, git-diffable, and easy
/// to share or manually curate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCard {
    pub id: String,
    pub session_id: String,
    pub project_path: Option<String>,
    pub created_at_secs: u64,
    pub title: String,
    pub problem: String,
    pub solution: String,
    pub key_patterns: Vec<String>,
    pub tags: Vec<String>,
    pub affected_files: Vec<String>,
}

impl MemoryCard {
    /// Returns all text fields concatenated in lowercase for keyword matching.
    pub fn searchable_text(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.title,
            self.problem,
            self.solution,
            self.tags.join(" "),
            self.key_patterns.join(" "),
        )
        .to_lowercase()
    }

    /// Render the card as a compact context hint suitable for injecting into a
    /// pipeline `RequestContext.session_state.persistent_summary`.
    pub fn as_context_hint(&self) -> String {
        let patterns = if self.key_patterns.is_empty() {
            String::new()
        } else {
            format!("\nKey patterns: {}", self.key_patterns.join(", "))
        };
        format!(
            "[Memory] {}\nProblem: {}\nSolution: {}{}",
            self.title, self.problem, self.solution, patterns,
        )
    }

    /// Render the card as a human-readable CLI display block.
    pub fn display_block(&self) -> String {
        let mut lines = vec![
            format!("  \x1b[1m{}\x1b[0m", self.title),
            format!("  Problem:   {}", self.problem),
            format!("  Solution:  {}", self.solution),
        ];
        if !self.tags.is_empty() {
            lines.push(format!("  Tags:      {}", self.tags.join(", ")));
        }
        if !self.key_patterns.is_empty() {
            lines.push(format!("  Patterns:  {}", self.key_patterns.join(", ")));
        }
        if !self.affected_files.is_empty() {
            lines.push(format!("  Files:     {}", self.affected_files.join(", ")));
        }
        lines.join("\n")
    }
}

// ──────────────────────────────────────────────
// DistillationIndex
// ──────────────────────────────────────────────

/// Tracks which sessions have been distilled and when.
///
/// Stored as `.harness/memories/index.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DistillationIndex {
    /// session_id → Unix timestamp of last successful distillation.
    pub distilled_at: HashMap<String, u64>,
}

// ──────────────────────────────────────────────
// LongTermMemoryStore
// ──────────────────────────────────────────────

/// File-backed store for long-term memory cards.
///
/// Layout:
/// ```text
/// .harness/memories/
///   index.json          — distillation index
///   mem-<id>.md         — one markdown file per memory card
/// ```
pub struct LongTermMemoryStore {
    root: PathBuf,
}

impl LongTermMemoryStore {
    pub fn for_project(project_dir: impl AsRef<Path>) -> Self {
        Self {
            root: project_dir.as_ref().join(".harness").join("memories"),
        }
    }

    /// Returns the project root directory (parent of `.harness/`).
    pub fn project_dir(&self) -> PathBuf {
        self.root
            .parent() // .harness/
            .and_then(|p| p.parent()) // project root
            .unwrap_or(&self.root)
            .to_path_buf()
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn card_path(&self, card_id: &str) -> PathBuf {
        self.root.join(format!("{card_id}.md"))
    }

    async fn ensure_dir(&self) -> Result<(), MemoryError> {
        fs::create_dir_all(&self.root).await?;
        Ok(())
    }

    // ── Index ──────────────────────────────────────────────────────────────

    pub async fn load_index(&self) -> Result<DistillationIndex, MemoryError> {
        match fs::read_to_string(self.index_path()).await {
            Ok(content) => Ok(serde_json::from_str(&content)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(DistillationIndex::default())
            }
            Err(e) => Err(MemoryError::Io(e)),
        }
    }

    pub async fn save_index(&self, index: &DistillationIndex) -> Result<(), MemoryError> {
        self.ensure_dir().await?;
        let content = serde_json::to_string_pretty(index)?;
        let tmp = self.index_path().with_extension("tmp");
        fs::write(&tmp, content).await?;
        fs::rename(&tmp, self.index_path()).await?;
        Ok(())
    }

    // ── Cards ──────────────────────────────────────────────────────────────

    /// Persist a `MemoryCard` as a markdown file (atomic write).
    pub async fn save_card(&self, card: &MemoryCard) -> Result<(), MemoryError> {
        self.ensure_dir().await?;
        let content = render_card_markdown(card);
        let path = self.card_path(&card.id);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content).await?;
        fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// Load all stored cards, sorted newest-first.
    pub async fn load_all(&self) -> Result<Vec<MemoryCard>, MemoryError> {
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(MemoryError::Io(e)),
        };

        let mut cards = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let content = match fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to read memory card; skipping");
                    continue;
                }
            };
            if let Some(card) = parse_card_markdown(&content) {
                cards.push(card);
            } else {
                warn!(path = %path.display(), "Failed to parse memory card markdown; skipping");
            }
        }

        cards.sort_by(|a, b| b.created_at_secs.cmp(&a.created_at_secs));

        // Back-fill project_path from the store location (not stored in markdown
        // so cards loaded from disk would otherwise have `None`).
        let project_path_str = self.project_dir().to_string_lossy().into_owned();
        for card in &mut cards {
            if card.project_path.is_none() {
                card.project_path = Some(project_path_str.clone());
            }
        }

        Ok(cards)
    }

    /// Keyword-based search. Returns cards with their hit counts, sorted by score desc.
    /// If `query` is empty, returns all cards with score 0.
    pub async fn keyword_search(&self, query: &str) -> Result<Vec<(MemoryCard, usize)>, MemoryError> {
        let cards = self.load_all().await?;
        if query.split_whitespace().next().is_none() {
            return Ok(cards.into_iter().map(|c| (c, 0)).collect());
        }
        let query_lower = query.to_lowercase();
        let words_lower: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(MemoryCard, usize)> = cards
            .into_iter()
            .filter_map(|card| {
                let text = card.searchable_text();
                let hits = words_lower.iter().filter(|&&w| text.contains(w)).count();
                if hits > 0 { Some((card, hits)) } else { None }
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(scored)
    }

    /// Keyword search followed by LLM reranking.
    ///
    /// Steps:
    /// 1. `keyword_search` returns up to `top_k * 2` candidates
    /// 2. LLM receives the candidates and the query, returns a ranked id list
    /// 3. Cards are reordered accordingly; top `top_k` are returned
    ///
    /// Falls back gracefully to keyword order if the LLM call fails.
    pub async fn recall_and_rerank(
        &self,
        llm: &Arc<dyn LlmProvider>,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryCard>, MemoryError> {
        let candidates = self.keyword_search(query).await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let cards: Vec<MemoryCard> = candidates
            .into_iter()
            .take(top_k * 2)
            .map(|(c, _)| c)
            .collect();

        if cards.len() <= 1 {
            return Ok(cards.into_iter().take(top_k).collect());
        }

        // Build a compact list for the LLM to reason over.
        let card_summaries: Vec<serde_json::Value> = cards
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "problem": c.problem,
                    "tags": c.tags,
                })
            })
            .collect();

        let prompt = format!(
            "User query: \"{query}\"\n\n\
             Rank these memory cards by relevance to the query. \
             Return ONLY a JSON array of ids in order from most to least relevant, \
             e.g. [\"mem-abc\", \"mem-def\"].\n\n\
             Cards:\n{}",
            serde_json::to_string_pretty(&card_summaries).unwrap_or_default(),
        );

        match llm.complete(&[LlmMessage::user(prompt)]).await {
            Ok(response) => {
                let ranked_ids = parse_string_array(&response.content);
                if ranked_ids.is_empty() {
                    return Ok(cards.into_iter().take(top_k).collect());
                }
                let mut reranked: Vec<MemoryCard> = ranked_ids
                    .iter()
                    .filter_map(|id| cards.iter().find(|c| &c.id == id).cloned())
                    .take(top_k)
                    .collect();
                // Fill any remaining slots with unranked cards in keyword order.
                for card in &cards {
                    if reranked.len() >= top_k {
                        break;
                    }
                    if !reranked.iter().any(|c| c.id == card.id) {
                        reranked.push(card.clone());
                    }
                }
                Ok(reranked)
            }
            Err(_) => {
                // LLM reranking failed — fall back to keyword order.
                Ok(cards.into_iter().take(top_k).collect())
            }
        }
    }
}

// ──────────────────────────────────────────────
// Markdown serialisation helpers
// ──────────────────────────────────────────────

pub(crate) fn render_card_markdown(card: &MemoryCard) -> String {
    let tags_yaml = if card.tags.is_empty() {
        "  []".to_string()
    } else {
        card.tags
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let patterns = if card.key_patterns.is_empty() {
        "(none)".to_string()
    } else {
        card.key_patterns.join(", ")
    };
    let files = if card.affected_files.is_empty() {
        "(none)".to_string()
    } else {
        card.affected_files.join(", ")
    };

    format!(
        "---\n\
         id: {}\n\
         session_id: {}\n\
         created: {}\n\
         tags:\n{}\n\
         ---\n\n\
         # {}\n\n\
         ## 问题\n{}\n\n\
         ## 解决方案\n{}\n\n\
         ## 关键模式\n{}\n\n\
         ## 涉及文件\n{}\n",
        card.id,
        card.session_id,
        card.created_at_secs,
        tags_yaml,
        card.title,
        card.problem,
        card.solution,
        patterns,
        files,
    )
}

fn parse_card_markdown(content: &str) -> Option<MemoryCard> {
    // Expect the file to start with "---\n"
    let after_open = content.strip_prefix("---\n")?;
    let end_fence = after_open.find("\n---\n")?;
    let frontmatter = &after_open[..end_fence];
    let body = &after_open[end_fence + 5..]; // skip "\n---\n"

    let id = yaml_str(frontmatter, "id")?;
    let session_id = yaml_str(frontmatter, "session_id").unwrap_or_default();
    let created_at_secs = yaml_u64(frontmatter, "created").unwrap_or(0);
    let tags = yaml_list(frontmatter, "tags");

    let title = extract_section_title(body, "# ")?;
    let problem = extract_between(body, "## 问题", "## 解决方案")
        .unwrap_or_default()
        .trim()
        .to_string();
    let solution = extract_between(body, "## 解决方案", "## 关键模式")
        .unwrap_or_default()
        .trim()
        .to_string();
    let patterns_raw = extract_between(body, "## 关键模式", "## 涉及文件")
        .unwrap_or_default();
    let files_raw = extract_after(body, "## 涉及文件").unwrap_or_default();

    // Patterns are stored as a comma-separated line (not a list)
    let key_patterns: Vec<String> = patterns_raw
        .trim()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "(none)")
        .collect();

    let affected_files: Vec<String> = files_raw
        .trim()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "(none)")
        .collect();

    Some(MemoryCard {
        id,
        session_id,
        project_path: None,
        created_at_secs,
        title,
        problem,
        solution,
        key_patterns,
        tags,
        affected_files,
    })
}

// ── YAML / Markdown tiny parsers ──────────────────────────────────────────────

fn yaml_str(frontmatter: &str, key: &str) -> Option<String> {
    for line in frontmatter.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn yaml_u64(frontmatter: &str, key: &str) -> Option<u64> {
    yaml_str(frontmatter, key)?.parse().ok()
}

fn yaml_list(frontmatter: &str, key: &str) -> Vec<String> {
    let mut in_list = false;
    let mut items = Vec::new();
    for line in frontmatter.lines() {
        let key_prefix = format!("{key}:");
        if line.starts_with(&key_prefix) {
            in_list = true;
            // Handle inline: `tags: [a, b]`
            if let Some(rest) = line.strip_prefix(&format!("{key}: [")) {
                return rest
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            continue;
        }
        if in_list {
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                items.push(item.trim().to_string());
            } else if !line.starts_with(' ') {
                break;
            }
        }
    }
    items
}

fn extract_section_title(body: &str, prefix: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim).map(str::to_string))
}

fn extract_between(body: &str, start_header: &str, end_header: &str) -> Option<String> {
    let start_marker = format!("{start_header}\n");
    let start = body.find(&start_marker).map(|i| i + start_marker.len())?;
    let end = body[start..]
        .find(&format!("\n{end_header}"))
        .map(|i| start + i)
        .unwrap_or(body.len());
    Some(body[start..end].to_string())
}

fn extract_after(body: &str, start_header: &str) -> Option<String> {
    let start_marker = format!("{start_header}\n");
    let start = body.find(&start_marker).map(|i| i + start_marker.len())?;
    Some(body[start..].to_string())
}

fn parse_string_array(text: &str) -> Vec<String> {
    // Find the JSON array in the response.
    let start = match text.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let end = match text[start..].find(']').map(|i| start + i + 1) {
        Some(e) => e,
        None => return Vec::new(),
    };
    let array_str = &text[start..end];
    match serde_json::from_str::<Vec<String>>(array_str) {
        Ok(ids) => ids,
        Err(_) => Vec::new(),
    }
}
