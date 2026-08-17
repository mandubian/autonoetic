use autonoetic_types::agent::{
    AgentEgressManifest, AgentIO, AgentIdentity, AgentManifest, CompressionConfig, ExecutionMode,
    LlmConfig, Middleware, ResourceLimits, RuntimeDeclaration, SandboxNetworkPolicy,
    ScriptInputMode,
};
use autonoetic_types::background::BackgroundPolicy;
use autonoetic_types::capability::Capability;
use autonoetic_types::disclosure::DisclosurePolicy;

pub struct TestManifest {
    manifest: AgentManifest,
}

impl Default for TestManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl TestManifest {
    pub fn new() -> Self {
        Self {
            manifest: AgentManifest {
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
                    id: "test-agent".to_string(),
                    name: "test-agent".to_string(),
                    description: "test agent".to_string(),
                    singleton: false,
                    resident_idle_ttl_secs: None,
                },
                capabilities: vec![],
                llm_overrides: None,
                llm_preset: None,
                llm_config: None,
                limits: None,
                background: None,
                disclosure: None,
                io: None,
                middleware: None,
                execution_mode: ExecutionMode::default(),
                script_entry: None,
                script_input_mode: ScriptInputMode::default(),
                gateway_url: None,
                gateway_token: None,
                allowed_tool_tiers: vec![],
                excluded_tools: vec![],
                sections: Vec::new(),
                agentskills_import: None,
                compression: None,
                open_web: false,
                sandbox_network: SandboxNetworkPolicy::default(),
                egress: None,
            },
        }
    }

    pub fn agent_id(mut self, id: &str) -> Self {
        self.manifest.agent.id = id.to_string();
        self.manifest.agent.name = id.to_string();
        self.manifest.agent.description = id.to_string();
        self
    }

    pub fn capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.manifest.capabilities = capabilities;
        self
    }

    pub fn sandbox_network(mut self, policy: SandboxNetworkPolicy) -> Self {
        self.manifest.sandbox_network = policy;
        self
    }

    pub fn execution_mode(mut self, mode: ExecutionMode) -> Self {
        self.manifest.execution_mode = mode;
        self
    }

    pub fn script_entry(mut self, entry: &str) -> Self {
        self.manifest.script_entry = Some(entry.to_string());
        self
    }

    pub fn llm_config(mut self, config: LlmConfig) -> Self {
        self.manifest.llm_config = Some(config);
        self
    }

    pub fn egress(mut self, egress: AgentEgressManifest) -> Self {
        self.manifest.egress = Some(egress);
        self
    }

    pub fn io(mut self, io: AgentIO) -> Self {
        self.manifest.io = Some(io);
        self
    }

    pub fn open_web(mut self, open: bool) -> Self {
        self.manifest.open_web = open;
        self
    }

    pub fn sandbox(mut self, sandbox: &str) -> Self {
        self.manifest.runtime.sandbox = sandbox.to_string();
        self
    }

    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.manifest.limits = Some(limits);
        self
    }

    pub fn background(mut self, background: BackgroundPolicy) -> Self {
        self.manifest.background = Some(background);
        self
    }

    pub fn disclosure(mut self, disclosure: DisclosurePolicy) -> Self {
        self.manifest.disclosure = Some(disclosure);
        self
    }

    pub fn middleware(mut self, middleware: Middleware) -> Self {
        self.manifest.middleware = Some(middleware);
        self
    }

    pub fn compression(mut self, compression: CompressionConfig) -> Self {
        self.manifest.compression = Some(compression);
        self
    }

    pub fn build(self) -> AgentManifest {
        self.manifest
    }
}
