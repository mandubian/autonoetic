use std::path::Path;

use autonoetic_types::background::{CodeExcerpt, RiskSummary};

use crate::ArtifactStore;
use crate::runtime::promotion_store::PromotionStore;

const MAX_TOTAL_BYTES: usize = 128 * 1024;
const MAX_PER_FILE_BYTES: usize = 32 * 1024;
const MAX_FILES: usize = 5;

fn language_from_path(path: &str) -> String {
    if path.ends_with(".py") || path.ends_with(".python") {
        return "python".to_string();
    }
    if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".cjs") {
        return "javascript".to_string();
    }
    if path.ends_with(".ts") || path.ends_with(".tsx") {
        return "typescript".to_string();
    }
    if path.ends_with(".rs") {
        return "rust".to_string();
    }
    if path.ends_with(".go") {
        return "go".to_string();
    }
    if path.ends_with(".sh") || path.ends_with(".bash") || path.ends_with(".zsh") {
        return "bash".to_string();
    }
    if path.ends_with(".yaml") || path.ends_with(".yml") {
        return "yaml".to_string();
    }
    if path.ends_with(".json") {
        return "json".to_string();
    }
    if path.ends_with(".md") || path.ends_with(".markdown") {
        return "markdown".to_string();
    }
    if path.ends_with(".html") || path.ends_with(".htm") {
        return "html".to_string();
    }
    if path.ends_with(".css") {
        return "css".to_string();
    }
    if path.ends_with(".toml") {
        return "toml".to_string();
    }
    "text".to_string()
}

fn is_executable_source(name: &str) -> bool {
    let name_lower = name.to_lowercase();
    !name_lower.contains("test") && !name_lower.contains("fixture") && !name_lower.contains("spec")
        && !name_lower.contains("lock") && !name_lower.starts_with('.')
}

/// Build code excerpts from an artifact. Returns `None` when the artifact
/// cannot be resolved or contains no eligible source files.
pub fn build_code_excerpts(
    artifact_id: &str,
    gateway_dir: &Path,
) -> Option<Vec<CodeExcerpt>> {
    let artifact_store = ArtifactStore::new(gateway_dir).ok()?;
    let _bundle = artifact_store.inspect(artifact_id).ok()?;
    let files = artifact_store.resolve_files(artifact_id).ok()?;

    let mut excerpts: Vec<CodeExcerpt> = Vec::new();
    let mut total_bytes = 0usize;

    for (name, content) in &files {
        if excerpts.len() >= MAX_FILES {
            break;
        }
        if !is_executable_source(name) {
            continue;
        }
        let language = language_from_path(name);
        let size_bytes = content.len();
        let (truncated_content, truncated_from) = if size_bytes > MAX_PER_FILE_BYTES {
            let head = &content[..MAX_PER_FILE_BYTES / 2];
            let tail = &content[content.len() - MAX_PER_FILE_BYTES / 4..];
            let truncated = format!(
                "{}…\n/* --- truncated {} bytes --- */\n\n{}",
                String::from_utf8_lossy(head),
                size_bytes - MAX_PER_FILE_BYTES,
                String::from_utf8_lossy(tail),
            );
            (truncated, Some(size_bytes))
        } else {
            (String::from_utf8_lossy(content).to_string(), None)
        };
        let new_total = total_bytes + truncated_content.len();
        if new_total > MAX_TOTAL_BYTES {
            break;
        }
        total_bytes = new_total;
        excerpts.push(CodeExcerpt {
            file_name: name.clone(),
            content: truncated_content,
            language,
            size_bytes,
            truncated: truncated_from.is_some(),
            truncated_from_bytes: truncated_from,
        });
    }

    if excerpts.is_empty() {
        return None;
    }
    Some(excerpts)
}

/// Build a risk summary from the artifact's remote-access analysis and
/// auditor promotion record.
pub fn build_risk_summary(
    detected_hosts: Option<&[String]>,
    promotion_store: Option<&PromotionStore>,
    artifact_id: &str,
    artifact_store: Option<&ArtifactStore>,
) -> Option<RiskSummary> {
    let host_count = detected_hosts.map(|h| h.len()).unwrap_or(0);

    let mut dangerous_patterns: Vec<String> = Vec::new();
    let mut protocol_mix: Vec<String> = Vec::new();

    if let Some(store) = artifact_store {
        if let Ok(files) = store.resolve_files(artifact_id) {
            for (name, content) in &files {
                let code_str = String::from_utf8_lossy(content);
                if code_str.contains("base64") && code_str.len() > 1024 {
                    let to_add = format!("base64 literal >1 KiB in {}", name);
                    if !dangerous_patterns.contains(&to_add) {
                        dangerous_patterns.push(to_add);
                    }
                }
                if code_str.contains("eval(") || code_str.contains("exec(") {
                    let to_add = format!("eval/exec in {}", name);
                    if !dangerous_patterns.contains(&to_add) {
                        dangerous_patterns.push(to_add);
                    }
                }
                if code_str.contains("subprocess") && code_str.contains("base64") {
                    if dangerous_patterns.iter().any(|p| p.contains(name)) {
                        // already flagged
                    } else {
                        dangerous_patterns.push(format!("subprocess+base64 combo in {}", name));
                    }
                }
                if code_str.contains("http://") {
                    if !protocol_mix.contains(&"http".to_string()) {
                        protocol_mix.push("http".to_string());
                    }
                }
                if code_str.contains("https://") {
                    if !protocol_mix.contains(&"https".to_string()) {
                        protocol_mix.push("https".to_string());
                    }
                }
            }
        }
    }

    if let Some(h) = detected_hosts {
        for host in h {
            if host.contains(':') {
                if !protocol_mix.contains(&"custom_port".to_string()) {
                    protocol_mix.push("custom_port".to_string());
                }
            }
        }
        if h.len() > 1 && !protocol_mix.contains(&"multi_host".to_string()) {
            protocol_mix.push("multi_host".to_string());
        }
    }

    let (auditor_verdict, auditor_findings_link) =
        if let Some(prom_store) = promotion_store {
            prom_store.get_promotion(artifact_id).map_or(
                (None, None),
                |record| {
                    let verdict = if record.auditor_pass {
                        Some("pass".to_string())
                    } else if record.auditor_findings.is_empty() {
                        Some("unable_to_evaluate".to_string())
                    } else {
                        Some("fail".to_string())
                    };
                    let findings_link = if !record.auditor_findings.is_empty() {
                        Some(format!("audit:{}", artifact_id))
                    } else {
                        None
                    };
                    (verdict, findings_link)
                },
            )
        } else {
            (None, None)
        };

    if host_count == 0
        && dangerous_patterns.is_empty()
        && protocol_mix.is_empty()
        && auditor_verdict.is_none()
    {
        return None;
    }

    Some(RiskSummary {
        host_count,
        protocol_mix,
        dangerous_patterns,
        auditor_verdict,
        auditor_findings_link,
    })
}
