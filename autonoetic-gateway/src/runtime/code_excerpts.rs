use std::path::Path;

use autonoetic_types::background::{CodeExcerpt, RiskSummary};

use crate::ArtifactStore;
use crate::runtime::promotion_store::PromotionStore;

const MAX_TOTAL_BYTES: usize = 128 * 1024;
const MAX_PER_FILE_BYTES: usize = 32 * 1024;
const MAX_FILES: usize = 5;

pub(crate) fn language_from_path(path: &str) -> String {
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

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Head/tail-bound a body to [`MAX_PER_FILE_BYTES`], keeping the head and the
/// tail with a truncation marker between. Returns
/// `(bounded_content, truncated, original_size)`.
fn bound_content(content: &str) -> (String, bool, Option<usize>) {
    let size = content.len();
    if size <= MAX_PER_FILE_BYTES {
        return (content.to_string(), false, None);
    }
    let head_end = floor_char_boundary(content, MAX_PER_FILE_BYTES / 2);
    let tail_start = floor_char_boundary(content, size - MAX_PER_FILE_BYTES / 4);
    let bounded = format!(
        "{}…\n/* --- truncated {} bytes --- */\n\n{}",
        &content[..head_end],
        size - MAX_PER_FILE_BYTES,
        &content[tail_start..]
    );
    (bounded, true, Some(size))
}

/// Single-file excerpt for ad-hoc command content analyzed at approval time —
/// the `python3 script.py` case where there is no artifact bundle to excerpt.
/// The remote-access analysis already loaded this source (sandbox's
/// `extract_code_for_analysis`); attaching it to the approval record is what
/// makes the `── code this runs ──` block possible for plain script gates.
/// `language_hint` backs the extension-derived language (scripts without a
/// recognizable extension).
pub fn script_code_excerpt(
    file_name: Option<&str>,
    content: &str,
    language_hint: &str,
) -> Option<CodeExcerpt> {
    let body = content.trim();
    if body.is_empty() {
        return None;
    }
    let language = file_name
        .map(language_from_path)
        .filter(|l| l.as_str() != "text")
        .unwrap_or_else(|| language_hint.to_string());
    let (bounded, truncated, truncated_from) = bound_content(body);
    Some(CodeExcerpt {
        file_name: file_name.unwrap_or("<inline>").to_string(),
        content: bounded,
        language,
        size_bytes: body.len(),
        truncated,
        truncated_from_bytes: truncated_from,
    })
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
        let text = String::from_utf8_lossy(content);
        let (truncated_content, truncated, truncated_from) = bound_content(&text);
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
            truncated,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_excerpt_passes_small_content_through() {
        let ex = script_code_excerpt(Some("/tmp/signal_self_test.py"), "print('hi')\n", "python")
            .expect("excerpt for non-empty content");
        assert_eq!(ex.file_name, "/tmp/signal_self_test.py");
        assert_eq!(ex.content, "print('hi')");
        assert_eq!(ex.language, "python");
        assert!(!ex.truncated);
        assert_eq!(ex.truncated_from_bytes, None);
        assert_eq!(ex.size_bytes, "print('hi')".len());
    }

    #[test]
    fn script_excerpt_bounds_large_content_head_and_tail() {
        let body = format!("{}\n{}\n", "a".repeat(40_000), "b".repeat(40_000));
        let ex = script_code_excerpt(Some("/tmp/big.py"), &body, "python")
            .expect("excerpt for non-empty content");
        assert!(ex.truncated);
        assert_eq!(ex.truncated_from_bytes, Some(body.trim().len()));
        assert!(ex.content.contains("truncated"));
        assert!(ex.content.starts_with("aaaa"));
        assert!(ex.content.ends_with("bbbb"));
    }

    #[test]
    fn script_excerpt_is_none_for_empty_content() {
        assert!(script_code_excerpt(Some("/tmp/s.py"), "   \n", "python").is_none());
    }

    #[test]
    fn script_excerpt_language_hint_fills_unrecognized_extension() {
        let ex = script_code_excerpt(Some("/tmp/run_me"), "x = 1", "python")
            .expect("excerpt for non-empty content");
        assert_eq!(ex.language, "python");
    }

    #[test]
    fn script_excerpt_never_panics_on_multibyte_boundary() {
        // 'é' is 2 bytes; 40_000 chars of it straddles every cut point.
        let body = "é".repeat(40_000);
        let ex = script_code_excerpt(Some("/tmp/unicode.py"), &body, "python")
            .expect("excerpt for non-empty content");
        assert!(ex.truncated);
    }
}
