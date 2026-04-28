//! Capability Policy Engine.
//!
//! Provides security validation for agent actions including:
//! - Command pattern matching against capability restrictions
//! - Security analysis for dangerous commands
//! - Path access validation for file operations

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

/// Security threat categories for command analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum SecurityThreat {
    /// Command can destroy data or filesystem (e.g., rm -rf /, dd)
    Destructive,
    /// Command attempts privilege escalation (e.g., sudo, su)
    PrivilegeEscalation,
    /// Command reads or prints environment/process secrets (e.g., env, printenv)
    EnvironmentDisclosure,
    /// Command may exfiltrate data or make unauthorized network calls
    NetworkExfiltration,
    /// Command attempts to escape sandbox (e.g., accessing /proc, /sys)
    SandboxEscape,
    /// Command may cause resource exhaustion (e.g., fork bomb)
    ResourceExhaustion,
    /// Command contains shell injection patterns (e.g., $(...), eval)
    ShellInjection,
    /// Command executes code from string/pipe (e.g., python -c, bash -c)
    CodeFromInput,
}

/// Result of security analysis.
#[derive(Debug, Clone)]
pub struct SecurityAnalysis {
    pub is_safe: bool,
    pub threats: Vec<SecurityThreat>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub enforced_rules: Vec<&'static str>,
    pub security_analysis: Option<SecurityAnalysis>,
}

impl PolicyDecision {
    pub fn allow(rule_id: &'static str) -> Self {
        Self {
            allowed: true,
            enforced_rules: vec![rule_id],
            security_analysis: None,
        }
    }

    pub fn deny(rule_id: &'static str) -> Self {
        Self {
            allowed: false,
            enforced_rules: vec![rule_id],
            security_analysis: None,
        }
    }

    pub fn deny_with_analysis(rule_id: &'static str, security_analysis: SecurityAnalysis) -> Self {
        Self {
            allowed: false,
            enforced_rules: vec![rule_id],
            security_analysis: Some(security_analysis),
        }
    }

    pub fn is_allowed(&self) -> bool {
        self.allowed
    }

    pub fn into_rule_ids(self) -> Vec<String> {
        self.enforced_rules
            .into_iter()
            .map(str::to_string)
            .collect()
    }
}

/// Analyzes shell commands for security threats.
pub struct SecurityAnalyzer;

impl SecurityAnalyzer {
    /// Analyze a command for security threats.
    /// Returns Analysis with threats found and whether it's safe to execute.
    pub fn analyze_command(command: &str) -> SecurityAnalysis {
        let mut threats = Vec::new();

        // Split command by shell separators to analyze each part
        let segments: Vec<&str> = command
            .split(|c| c == '|' || c == '&' || c == ';')
            .collect();

        for segment in &segments {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Check for destructive commands
            if Self::is_destructive(trimmed) {
                threats.push(SecurityThreat::Destructive);
            }

            // Check for privilege escalation
            if Self::is_privilege_escalation(trimmed) {
                threats.push(SecurityThreat::PrivilegeEscalation);
            }

            // Check for environment disclosure patterns
            if Self::is_environment_disclosure(trimmed) {
                threats.push(SecurityThreat::EnvironmentDisclosure);
            }

            // Check for sandbox escape attempts
            if Self::is_sandbox_escape(trimmed) {
                threats.push(SecurityThreat::SandboxEscape);
            }

            // Check for shell injection
            if Self::is_shell_injection(trimmed) {
                threats.push(SecurityThreat::ShellInjection);
            }

            // Check for code execution from input
            if Self::is_code_from_input(trimmed) {
                threats.push(SecurityThreat::CodeFromInput);
            }

            // Check for resource exhaustion
            if Self::is_resource_exhaustion(trimmed) {
                threats.push(SecurityThreat::ResourceExhaustion);
            }
        }

        let is_safe = threats.is_empty();
        let reason = if !threats.is_empty() {
            Some(format!("Command contains security threats: {:?}", threats))
        } else {
            None
        };

        SecurityAnalysis {
            is_safe,
            threats,
            reason,
        }
    }

    /// True when a command *segment* starts with the Windows disk-formatter (`format C:`),
    /// not CLI flags like `--format json` (substring `"format "` matched those).
    fn segment_starts_with_disk_format_command(segment: &str) -> bool {
        let first = segment
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        first == "format"
            || first.ends_with("/format.com")
            || first.ends_with("\\format.com")
    }

    /// Check for destructive commands that can destroy data.
    fn is_destructive(cmd: &str) -> bool {
        let cmd_lower = cmd.to_lowercase();

        // Block direct destructive shell/file operations, even outside extreme forms like rm -rf /
        if Self::contains_shell_word(&cmd_lower, "rm")
            || Self::contains_shell_word(&cmd_lower, "rmdir")
            || Self::contains_shell_word(&cmd_lower, "unlink")
            || cmd_lower.contains("find ") && cmd_lower.contains(" -delete")
        {
            return true;
        }

        for segment in cmd.split(|c| c == '|' || c == '&' || c == ';') {
            let trimmed = segment.trim();
            if !trimmed.is_empty() && Self::segment_starts_with_disk_format_command(trimmed) {
                return true;
            }
        }

        let destructive_patterns = &[
            "dd if=",
            "dd of=/dev/",
            "mkfs",
            ":(){ :|:& };:",
            "> /dev/",
            "shred ",
            "wipefs",
        ];

        destructive_patterns.iter().any(|p| cmd_lower.contains(p))
    }

    /// Check for privilege escalation attempts.
    fn is_privilege_escalation(cmd: &str) -> bool {
        let cmd_lower = cmd.to_lowercase();

        if Self::contains_shell_word(&cmd_lower, "sudo")
            || Self::contains_shell_word(&cmd_lower, "su")
            || Self::contains_shell_word(&cmd_lower, "doas")
        {
            return true;
        }

        let escalation_patterns = &[
            "setuid",
            "setgid",
            "chmod +s",
            "chmod u+s",
            "chown root",
            "visudo",
        ];

        escalation_patterns.iter().any(|p| cmd_lower.contains(p))
    }

    /// Check for environment disclosure patterns.
    fn is_environment_disclosure(cmd: &str) -> bool {
        let cmd_lower = cmd.to_lowercase();

        if Self::contains_shell_word(&cmd_lower, "env")
            || cmd_lower.contains("printenv")
            || cmd_lower.contains("declare -x")
            || cmd_lower.contains("/proc/self/environ")
            || cmd_lower.contains("/proc/1/environ")
            || cmd_lower.contains("/etc/environment")
        {
            return true;
        }

        false
    }

    /// Check for sandbox escape attempts.
    fn is_sandbox_escape(cmd: &str) -> bool {
        let escape_patterns = &[
            "cat /proc/",
            "ls /proc/",
            "cat /sys/",
            "ls /sys/",
            "mount",
            "umount",
            "chroot",
            "nsenter",
            "unshare",
            "docker ",
            "lxc-",
            "systemctl",
            "service ",
        ];

        let cmd_lower = cmd.to_lowercase();
        escape_patterns.iter().any(|p| cmd_lower.contains(p))
    }

    /// Check for shell injection patterns.
    fn is_shell_injection(cmd: &str) -> bool {
        // Check for $(...) but allow $VAR in quotes
        if cmd.contains("$(") || cmd.contains("`") {
            // Allow common safe patterns like $(pwd), $(dirname $0) in scripts
            // For now, flag as potential threat - can be refined
            let safe_patterns = ["$(pwd)", "$(dirname", "$(basename"];
            if !safe_patterns.iter().any(|p| cmd.contains(p)) {
                return true;
            }
        }

        // Check for eval with user input
        if cmd.contains("eval ") {
            return true;
        }

        false
    }

    /// Check for code execution from string input (high risk).
    /// Note: python3 -c, bash -c, sh -c are NOT flagged here because they're
    /// already controlled by CodeExecution capability patterns.
    fn is_code_from_input(cmd: &str) -> bool {
        let code_patterns = &[
            // Less common/higher risk patterns
            "node -e ",
            "node --eval ",
            "perl -e ",
            "ruby -e ",
            "php -r ",
            "lua -e ",
        ];

        code_patterns.iter().any(|p| cmd.contains(p))
    }

    /// Check for resource exhaustion attacks.
    fn is_resource_exhaustion(cmd: &str) -> bool {
        let exhaustion_patterns = &[
            ":(){ :|:& };:", // Fork bomb
            "while true",
            "while :",
            "for (( ;; ))",
            "ulimit -c unlimited",
        ];

        exhaustion_patterns.iter().any(|p| cmd.contains(p))
    }

    pub(crate) fn contains_shell_word(cmd: &str, word: &str) -> bool {
        let mut offset = 0usize;
        while let Some(found) = cmd[offset..].find(word) {
            let start = offset + found;
            let end = start + word.len();

            let prev = if start == 0 {
                None
            } else {
                cmd[..start].chars().next_back()
            };
            let next = if end >= cmd.len() {
                None
            } else {
                cmd[end..].chars().next()
            };

            let prev_is_boundary = prev.map(Self::is_word_boundary).unwrap_or(true);
            let next_is_boundary = next.map(Self::is_word_boundary).unwrap_or(true);

            if prev_is_boundary && next_is_boundary {
                return true;
            }
            offset = end;
        }
        false
    }

    fn is_word_boundary(ch: char) -> bool {
        !ch.is_ascii_alphanumeric() && ch != '_'
    }

    /// Analyze Python script content for security threats.
    /// Returns threats found in the script code itself.
    pub fn analyze_script_content(script_content: &str) -> Vec<SecurityThreat> {
        let mut threats = Vec::new();

        // Network access patterns in Python
        let network_patterns = &[
            "urllib.request",
            "urllib.urlopen",
            "requests.get",
            "requests.post",
            "http.client",
            "httpx",
            "aiohttp",
            "socket.socket",
            "subprocess",
            "os.system",
            "os.popen",
        ];

        for pattern in network_patterns {
            if script_content.contains(pattern) {
                threats.push(SecurityThreat::NetworkExfiltration);
                break;
            }
        }

        // Code execution patterns
        let exec_patterns = &[
            "eval(",
            "exec(",
            "__import__(",
            "compile(",
            "getattr(__builtins__",
        ];

        for pattern in exec_patterns {
            if script_content.contains(pattern) {
                threats.push(SecurityThreat::ShellInjection);
                break;
            }
        }

        // File system destruction
        let fs_patterns = &[
            "shutil.rmtree",
            "os.remove(\"/\")",
            "os.unlink",
            "open('/dev/",
        ];

        for pattern in fs_patterns {
            if script_content.contains(pattern) {
                threats.push(SecurityThreat::Destructive);
                break;
            }
        }

        threats
    }

    /// Check if a Python script needs approval based on its content.
    /// Returns Some(reason) if approval is required, None if safe.
    pub fn script_requires_approval(
        script_content: &str,
        has_network_access: bool,
    ) -> Option<String> {
        let threats = Self::analyze_script_content(script_content);

        if threats.is_empty() {
            return None;
        }

        // Check if NetworkAccess capability would cover network calls
        if threats.contains(&SecurityThreat::NetworkExfiltration) && !has_network_access {
            return Some(
                "Script makes network calls but agent lacks NetworkAccess capability".to_string(),
            );
        }

        // Always require approval for these threats
        if threats.contains(&SecurityThreat::ShellInjection) {
            return Some("Script uses eval/exec which could be dangerous".to_string());
        }

        if threats.contains(&SecurityThreat::Destructive) {
            return Some("Script performs potentially destructive file operations".to_string());
        }

        None
    }
}

/// Validates requested actions against the Agent's configured capabilities.
pub struct PolicyEngine {
    manifest: AgentManifest,
}

impl PolicyEngine {
    pub fn new(manifest: AgentManifest) -> Self {
        Self { manifest }
    }

    /// Check if the agent is allowed to execute a given command string.
    /// First runs security analysis, then checks against capability patterns.
    pub fn can_exec_shell_detailed(&self, command: &str) -> PolicyDecision {
        // First, run security analysis
        let security = SecurityAnalyzer::analyze_command(command);
        if !security.is_safe {
            return PolicyDecision::deny_with_analysis("R-3.8", security);
        }

        // Then check against capability patterns
        for cap in &self.manifest.capabilities {
            if let Capability::CodeExecution { patterns, .. } = cap {
                let command_segments: Vec<&str> = command
                    .split(|c| c == '|' || c == '&' || c == ';')
                    .collect();

                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');

                    for segment in &command_segments {
                        let trimmed = segment.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if trimmed.starts_with(prefix) {
                            return PolicyDecision::allow("R-1.9");
                        }
                    }
                }
            }
        }

        // Check capability commands list (word-boundary matching on first word)
        for cap in &self.manifest.capabilities {
            if let Capability::CodeExecution { commands, .. } = cap {
                if !commands.is_empty() {
                    let command_segments: Vec<&str> = command
                        .split(|c| c == '|' || c == '&' || c == ';')
                        .collect();

                    for segment in &command_segments {
                        let trimmed = segment.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        for cmd in commands {
                            if SecurityAnalyzer::contains_shell_word(trimmed, cmd) {
                                return PolicyDecision::allow("R-1.9");
                            }
                        }
                    }
                }
            }
        }

        PolicyDecision::deny("R-1.9")
    }

    /// Check if the agent is allowed to execute a given command string.
    pub fn can_exec_shell(&self, command: &str) -> PolicyDecision {
        self.can_exec_shell_detailed(command)
    }

    /// Check if the agent is allowed to connect to a specific host.
    pub fn can_connect_net(&self, host: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::NetworkAccess { hosts } = cap {
                if hosts.iter().any(|h| h == host || h == "*") {
                    return PolicyDecision::allow("R-1.5");
                }
            }
        }
        PolicyDecision::deny("R-1.5")
    }

    /// Check if the agent is allowed to invoke a named tool (typically MCP tools).
    pub fn can_invoke_tool(&self, tool_name: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::SandboxFunctions { allowed } = cap {
                for pattern in allowed {
                    let prefix = pattern.trim_end_matches('*');
                    if tool_name.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.1");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.1")
    }

    /// Check if the agent is allowed to read from a relative file path.
    pub fn can_read_path(&self, path: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::ReadAccess { scopes } = cap {
                for scope in scopes {
                    let prefix = scope.trim_end_matches('*');
                    if path.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.4");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.4")
    }

    /// Check if the agent is allowed to write to a relative file path.
    pub fn can_write_path(&self, path: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::WriteAccess { scopes } = cap {
                for scope in scopes {
                    let prefix = scope.trim_end_matches('*');
                    if path.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.4");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.4")
    }

    /// Check if the agent is allowed to spawn child agents.
    pub fn can_spawn_agent(&self) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if matches!(cap, Capability::AgentSpawn { .. }) {
                return PolicyDecision::allow("R-1.7");
            }
        }
        PolicyDecision::deny("R-1.7")
    }

    /// Privileged: request gateway emergency stop for a root session.
    pub fn can_request_emergency_stop(&self) -> PolicyDecision {
        if self
            .manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::EmergencyStop))
        {
            PolicyDecision::allow("R-7.1")
        } else {
            PolicyDecision::deny("R-7.1")
        }
    }

    /// Return the configured child-agent delegation limit, if any.
    pub fn spawn_agent_limit(&self) -> Option<u32> {
        self.manifest.capabilities.iter().find_map(|cap| {
            if let Capability::AgentSpawn { max_children, .. } = cap {
                Some(*max_children)
            } else {
                None
            }
        })
    }

    /// Return the configured per-agent spawn-depth limit, if any.
    /// A value of 0 means "use the system default ceiling".
    pub fn spawn_depth_limit(&self) -> Option<u32> {
        self.manifest.capabilities.iter().find_map(|cap| {
            if let Capability::AgentSpawn { max_spawn_depth, .. } = cap {
                Some(*max_spawn_depth)
            } else {
                None
            }
        })
    }

    /// Check if the agent is allowed to message a target agent.
    pub fn can_message_agent(&self, target_agent: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::AgentMessage { patterns } = cap {
                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');
                    if target_agent.starts_with(prefix) {
                        return PolicyDecision::allow("R-11.5");
                    }
                }
            }
        }
        PolicyDecision::deny("R-11.5")
    }

    pub fn can_agent_revision(&self, target: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::AgentRevision { patterns } = cap {
                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');
                    if target.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.3");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.3")
    }

    pub fn can_evaluate_suite(&self, suite_id: &str, subject_agent_id: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::Evaluation { patterns } = cap {
                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');
                    if suite_id.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.1");
                    }
                    if !subject_agent_id.is_empty() && subject_agent_id.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.1");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.1")
    }

    pub fn can_evaluate_suite_publish(&self, suite_name: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::Evaluation { patterns } = cap {
                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');
                    if suite_name.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.1");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.1")
    }

    /// Return background reevaluation limits, if configured.
    pub fn background_reevaluation_limits(&self) -> Option<(u64, bool)> {
        self.manifest.capabilities.iter().find_map(|cap| {
            if let Capability::BackgroundReevaluation {
                min_interval_secs,
                allow_reasoning,
            } = cap
            {
                Some((*min_interval_secs, *allow_reasoning))
            } else {
                None
            }
        })
    }

    /// Check if the agent is allowed to search memory.
    /// Searching is included in ReadAccess capability.
    pub fn can_search_memory(&self, scope: &str) -> PolicyDecision {
        // Search uses the same scopes as read
        self.can_read_memory_scope(scope)
    }

    /// Check if the agent can write to a Tier 2 memory scope.
    pub fn can_write_memory_scope(&self, scope: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::WriteAccess { scopes } = cap {
                // Wildcard allows all scopes
                if scopes
                    .iter()
                    .any(|s| s == "*" || s.trim_end_matches('*').is_empty())
                {
                    return PolicyDecision::allow("R-1.4");
                }
                for allowed_scope in scopes {
                    let prefix = allowed_scope.trim_end_matches('*');
                    if scope.starts_with(prefix) || scope == allowed_scope {
                        return PolicyDecision::allow("R-1.4");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.4")
    }

    /// Check if the agent can read from a Tier 2 memory scope.
    pub fn can_read_memory_scope(&self, scope: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::ReadAccess { scopes } = cap {
                // Wildcard allows all scopes
                if scopes
                    .iter()
                    .any(|s| s == "*" || s.trim_end_matches('*').is_empty())
                {
                    return PolicyDecision::allow("R-1.4");
                }
                for allowed_scope in scopes {
                    let prefix = allowed_scope.trim_end_matches('*');
                    if scope.starts_with(prefix) || scope == allowed_scope {
                        return PolicyDecision::allow("R-1.4");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.4")
    }

    /// Check if the agent is allowed to perform a scheduler/cron operation.
    pub fn can_schedule(&self, operation: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::SchedulerAccess { patterns } = cap {
                for pattern in patterns {
                    let prefix = pattern.trim_end_matches('*');
                    if operation.starts_with(prefix) {
                        return PolicyDecision::allow("R-1.1");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.1")
    }

    /// Check if the agent is allowed to install a skill from a given URL host.
    pub fn can_install_skill(&self, url_host: &str) -> PolicyDecision {
        for cap in &self.manifest.capabilities {
            if let Capability::SkillInstall { allowed_sources } = cap {
                for source in allowed_sources {
                    if source == "*" || source == url_host {
                        return PolicyDecision::allow("R-1.1");
                    }
                }
            }
        }
        PolicyDecision::deny("R-1.1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};

    fn manifest_with_caps(capabilities: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "policy-test".to_string(),
                name: "policy-test".to_string(),
                description: "test".to_string(),
            },
            capabilities,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,

            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
        }
    }

    #[test]
    fn test_can_invoke_tool_exact_and_wildcard() {
        let manifest = manifest_with_caps(vec![Capability::SandboxFunctions {
            allowed: vec!["mcp_web_search".to_string(), "mcp_docs_*".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);

        assert!(policy.can_invoke_tool("mcp_web_search").is_allowed());
        assert!(policy.can_invoke_tool("mcp_docs_fetch").is_allowed());
        assert!(!policy.can_invoke_tool("mcp_web_fetch").is_allowed());
    }

    #[test]
    fn test_can_invoke_tool_denied_without_capability() {
        let manifest = manifest_with_caps(vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);
        assert!(!policy.can_invoke_tool("mcp_web_search").is_allowed());
    }

    // SecurityAnalyzer tests
    #[test]
    fn test_security_analyzer_clean_command() {
        let analysis = SecurityAnalyzer::analyze_command("python3 script.py");
        assert!(analysis.is_safe);
        assert!(analysis.threats.is_empty());
    }

    #[test]
    fn test_security_analyzer_pipe_command() {
        let analysis = SecurityAnalyzer::analyze_command("echo hello | python3 process.py");
        assert!(analysis.is_safe);
    }

    #[test]
    fn test_security_analyzer_destructive_rm() {
        let analysis = SecurityAnalyzer::analyze_command("rm -rf /");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }

    #[test]
    fn test_security_analyzer_destructive_rm_file() {
        let analysis = SecurityAnalyzer::analyze_command("rm /tmp/test.txt");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }

    #[test]
    fn test_security_analyzer_destructive_dd() {
        let analysis = SecurityAnalyzer::analyze_command("dd if=/dev/zero of=/dev/sda");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }

    #[test]
    fn test_security_analyzer_format_flag_not_destructive() {
        // Regression: substring "format " falsely matched --format (e.g. argparse).
        let analysis = SecurityAnalyzer::analyze_command(
            "python3 weather.py --location London --date 2025-01-15 --format json",
        );
        assert!(analysis.is_safe, "{:?}", analysis.threats);
    }

    #[test]
    fn test_security_analyzer_windows_format_command_destructive() {
        let analysis = SecurityAnalyzer::analyze_command("format C:");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }

    #[test]
    fn test_security_analyzer_privilege_escalation() {
        let analysis = SecurityAnalyzer::analyze_command("sudo rm /etc/passwd");
        assert!(!analysis.is_safe);
        assert!(analysis
            .threats
            .contains(&SecurityThreat::PrivilegeEscalation));
    }

    #[test]
    fn test_security_analyzer_environment_disclosure_env() {
        let analysis = SecurityAnalyzer::analyze_command("bash -c 'env'");
        assert!(!analysis.is_safe);
        assert!(analysis
            .threats
            .contains(&SecurityThreat::EnvironmentDisclosure));
    }

    #[test]
    fn test_security_analyzer_environment_disclosure_printenv() {
        let analysis = SecurityAnalyzer::analyze_command("printenv");
        assert!(!analysis.is_safe);
        assert!(analysis
            .threats
            .contains(&SecurityThreat::EnvironmentDisclosure));
    }

    #[test]
    fn test_security_analyzer_sandbox_escape() {
        let analysis = SecurityAnalyzer::analyze_command("cat /proc/self/status");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::SandboxEscape));
    }

    #[test]
    fn test_security_analyzer_code_from_input() {
        // python3 -c is allowed (controlled by CodeExecution patterns)
        // but node -e is still blocked as high risk
        let analysis =
            SecurityAnalyzer::analyze_command("node -e 'require(\"child_process\").exec(\"ls\")'");
        assert!(!analysis.is_safe);
        assert!(analysis.threats.contains(&SecurityThreat::CodeFromInput));
    }

    #[test]
    fn test_security_analyzer_python_c_allowed() {
        // python3 -c should NOT be flagged - controlled by CodeExecution patterns
        let analysis = SecurityAnalyzer::analyze_command("python3 -c 'print(\"hello\")'");
        assert!(analysis.is_safe);
    }

    #[test]
    fn test_security_analyzer_pipe_with_safe_python() {
        // This is the case that was failing - piped python should be safe
        let analysis = SecurityAnalyzer::analyze_command(
            "echo '{\"place\": \"London\"}' | python3 weather.py",
        );
        assert!(analysis.is_safe);
    }

    #[test]
    fn test_policy_allows_safe_bash_when_pattern_matches() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec!["bash -c ".to_string()],
            commands: vec![],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("bash -c 'printf hello'");
        assert!(decision.is_allowed());
        assert!(decision.security_analysis.is_none());
    }

    #[test]
    fn test_policy_denies_bash_rm_even_when_pattern_matches() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec!["bash -c ".to_string()],
            commands: vec![],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("bash -c 'rm /tmp/a'");
        assert!(!decision.is_allowed());
        let analysis = decision
            .security_analysis
            .expect("security analysis should be present for denial");
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }

    #[test]
    fn test_policy_denies_bash_printenv_even_when_pattern_matches() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec!["bash -c ".to_string()],
            commands: vec![],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("bash -c 'printenv'");
        assert!(!decision.is_allowed());
        let analysis = decision
            .security_analysis
            .expect("security analysis should be present for denial");
        assert!(analysis
            .threats
            .contains(&SecurityThreat::EnvironmentDisclosure));
    }

    #[test]
    fn test_policy_allows_command_when_in_commands_list() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec!["python3 ".to_string()],
            commands: vec!["date".to_string(), "ls".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("date");
        assert!(decision.is_allowed());
        assert!(decision.security_analysis.is_none());

        let decision = policy.can_exec_shell_detailed("ls -la /tmp");
        assert!(decision.is_allowed());
        assert!(decision.security_analysis.is_none());
    }

    #[test]
    fn test_policy_denies_command_not_in_commands_list() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec!["python3 ".to_string()],
            commands: vec!["date".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("whoami");
        assert!(!decision.is_allowed());
    }

    #[test]
    fn test_policy_commands_word_boundary_matching() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec![],
            commands: vec!["ls".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("ls -la");
        assert!(decision.is_allowed());

        let decision = policy.can_exec_shell_detailed("lsof");
        assert!(!decision.is_allowed());
    }

    #[test]
    fn test_policy_commands_security_blocks_first() {
        let manifest = manifest_with_caps(vec![Capability::CodeExecution {
            patterns: vec![],
            commands: vec!["rm".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest);

        let decision = policy.can_exec_shell_detailed("rm /tmp/a");
        assert!(!decision.is_allowed());
        let analysis = decision
            .security_analysis
            .expect("security analysis should be present");
        assert!(analysis.threats.contains(&SecurityThreat::Destructive));
    }
}
