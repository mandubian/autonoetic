//! XML-style tool call extraction from raw text.
//!
//! When models use XML-style templates (like Qwen 3.5's qwen35-template.jinja)
//! that produce `<tool_call><function=...><parameter=...>...</tool_call>` in the
//! response text, the LLM provider may not return structured `tool_calls` in
//! the JSON response body. This parser extracts them from the plain-text content.
//!
//! Format:
//! ```text
//! <tool_call>
//! <function=function_name>
//! <parameter=param_name>
//! param_value (can span multiple lines)
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! Multiple tool calls may appear sequentially. Optional reasoning text may
//! appear before the first `<tool_call>` block.

use crate::llm::ToolCall;

/// Remove a non-XML prefix from text. Returns the XML portion after keeping
/// any reasoning text that preceded the first `<tool_call>`.
fn strip_optional_prefix(text: &str) -> &str {
    if let Some(pos) = text.find("<tool_call>") {
        &text[pos..]
    } else {
        text
    }
}

/// Extract the function name from `<function=name>`.
fn parse_function(line: &str) -> Option<&str> {
    let s = line.trim();
    if s.len() > 10 && s.starts_with("<function=") && s.ends_with(">") {
        Some(&s[10..s.len() - 1])
    } else {
        None
    }
}

/// Extract the parameter name from `<parameter=name>`.
fn parse_parameter(line: &str) -> Option<&str> {
    let s = line.trim();
    if s.len() > 11 && s.starts_with("<parameter=") && s.ends_with(">") {
        Some(&s[11..s.len() - 1])
    } else {
        None
    }
}

/// Extract XML-style tool calls from raw response text.
///
/// Returns `(reasoning_prefix, tool_calls)` where:
/// - `reasoning_prefix` is any text before the first `<tool_call>` block
/// - `tool_calls` are the parsed tool invocations
pub fn extract_xml_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    // Collect reasoning text (everything before the first <tool_call>)
    // before we strip the prefix.
    let reasoning_end = text.find("<tool_call>").unwrap_or(text.len());
    let reasoning = if reasoning_end > 0 {
        text[..reasoning_end].trim().to_string()
    } else {
        String::new()
    };

    let text = strip_optional_prefix(text);

    if !text.starts_with("<tool_call>") {
        return (reasoning, vec![]);
    }

    let mut calls = Vec::new();
    let mut call_index: usize = 0;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line == "<tool_call>" {
            i += 1;
            let function_name = loop {
                if i >= lines.len() {
                    return (reasoning, calls);
                }
                let l = lines[i].trim();
                if l == "</tool_call>" && function_name_tag_already_seen(&calls, call_index) {
                    // Stray </tool_call> without closing </function> — skip.
                    i += 1;
                    break None;
                }
                if let Some(name) = parse_function(l) {
                    i += 1;
                    break Some(name.to_string());
                }
                i += 1;
            };

            let function_name = match function_name {
                Some(n) => n,
                None => continue,
            };

            // Generate a unique call ID for this tool call.
            let call_id = format!("xml_call_{}", call_index);
            call_index += 1;

            let mut args: serde_json::Map<String, serde_json::Value> =
                serde_json::Map::new();

            loop {
                if i >= lines.len() {
                    break;
                }
                let l = lines[i].trim();
                if l == "</function>" {
                    i += 1;
                    break;
                }
                if l == "<tool_call>" {
                    // Next tool call starts — close current one implicitly.
                    break;
                }
                if let Some(param_name) = parse_parameter(l) {
                    i += 1;
                    let mut value_lines: Vec<&str> = Vec::new();
                    while i < lines.len() {
                        let inner = lines[i];
                        let trimmed = inner.trim();
                        if trimmed == "</parameter>" {
                            i += 1;
                            break;
                        }
                        if trimmed == "</function>" {
                            // Implicit parameter close
                            break;
                        }
                        if trimmed.starts_with("<parameter=") || trimmed.starts_with("<function=")
                        {
                            break;
                        }
                        value_lines.push(inner);
                        i += 1;
                    }
                    let value = value_lines.join("\n");
                    args.insert(
                        param_name.to_string(),
                        serde_json::Value::String(value),
                    );
                } else {
                    i += 1;
                }
            }

            // Read closing </tool_call> if present.
            if i < lines.len() && lines[i].trim() == "</tool_call>" {
                i += 1;
            }

            let arguments = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            calls.push(ToolCall {
                id: call_id,
                name: function_name,
                arguments,
            });
        } else {
            i += 1;
        }
    }

    (reasoning, calls)
}

fn function_name_tag_already_seen(calls: &[ToolCall], idx: usize) -> bool {
    calls.len() > idx
}

/// Format a Vec of ToolDefinitions as an XML-style tools block for the system message.
///
/// Produces output matching the Qwen 3.5 chat template format:
/// ```text
/// # Tools
///
/// You have access to the following functions:
///
/// <tools>
/// {"name": "...", "description": "...", "parameters": {...}}
/// </tools>
///
/// If you choose to call a function ONLY reply in the following format...
/// ```
pub fn render_xml_tool_definitions(
    tools: &[crate::llm::ToolDefinition],
) -> String {
    let mut s = String::new();
    s.push_str("# Tools\n\n");
    s.push_str("You have access to the following functions:\n\n");
    s.push_str("<tools>\n");
    for tool in tools {
        let json = serde_json::json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            }
        });
        s.push_str(&serde_json::to_string(&json).unwrap_or_default());
        s.push('\n');
    }
    s.push_str("</tools>\n\n");
    s.push_str("If you choose to call a function ONLY reply in the following format with NO suffix:\n\n");
    s.push_str("<tool_call>\n");
    s.push_str("<function=example_function_name>\n");
    s.push_str("<parameter=example_parameter_1>\n");
    s.push_str("value_1\n");
    s.push_str("</parameter>\n");
    s.push_str("<parameter=example_parameter_2>\n");
    s.push_str("This is the value for the second parameter\n");
    s.push_str("that can span\n");
    s.push_str("multiple lines\n");
    s.push_str("</parameter>\n");
    s.push_str("</function>\n");
    s.push_str("</tool_call>\n\n");
    s.push_str("<IMPORTANT>\n");
    s.push_str("Reminder:\n");
    s.push_str("- Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n");
    s.push_str("- Required parameters MUST be specified\n");
    s.push_str("- You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n");
    s.push_str("- If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls\n");
    s.push_str("</IMPORTANT>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_single_tool_call() {
        let text = r#"Some reasoning text.

<tool_call>
<function=content_write>
<parameter=name>
my_file.py
</parameter>
<parameter=content>
print("hello")
</parameter>
</function>
</tool_call>"#;

        let (reasoning, calls) = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "content_write");
        assert!(reasoning.contains("Some reasoning"));
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["name"], "my_file.py");
        assert_eq!(args["content"], "print(\"hello\")");
    }

    #[test]
    fn test_extract_multiple_tool_calls() {
        let text = r#"<tool_call>
<function=content_write>
<parameter=name>
a.py
</parameter>
<parameter=content>
code a
</parameter>
</function>
</tool_call>
<tool_call>
<function=content_write>
<parameter=name>
b.py
</parameter>
<parameter=content>
code b
</parameter>
</function>
</tool_call>"#;

        let (_reasoning, calls) = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "content_write");
        assert_eq!(calls[1].name, "content_write");
    }

    #[test]
    fn test_no_tool_calls() {
        let text = "Just a normal response without any tool calls.";
        let (_reasoning, calls) = extract_xml_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_multiline_parameter_value() {
        let text = r#"<tool_call>
<function=content_write>
<parameter=name>
script.py
</parameter>
<parameter=content>
import os
import sys

def main():
    print("hello world")
</parameter>
</function>
</tool_call>"#;

        let (_reasoning, calls) = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        let content = args["content"].as_str().unwrap();
        assert!(content.contains("import os"));
        assert!(content.contains("print(\"hello world\")"));
    }

    #[test]
    fn test_render_xml_tool_definitions() {
        let tools = vec![crate::llm::ToolDefinition {
            name: "content_write".to_string(),
            description: "Write a file".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["name", "content"],
            }),
        }];
        let rendered = render_xml_tool_definitions(&tools);
        assert!(rendered.contains("<tools>"));
        assert!(rendered.contains("content_write"));
        assert!(rendered.contains("</tool_call>"));
    }
}
