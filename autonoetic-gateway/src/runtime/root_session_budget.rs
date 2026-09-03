use autonoetic_types::config::RootSessionBudgetConfig;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Default, Clone)]
struct TreeCounters {
    llm_rounds: u64,
    tool_invocations: u64,
    llm_tokens: u64,
    session_cost_usd: f64,
    clock_start: Option<Instant>,
}

#[derive(Debug)]
pub struct RootSessionBudgetRegistry {
    limits: RootSessionBudgetConfig,
    trees: Mutex<HashMap<String, TreeCounters>>,
}

impl RootSessionBudgetRegistry {
    pub fn new(limits: RootSessionBudgetConfig) -> Self {
        Self {
            limits,
            trees: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.limits.max_llm_rounds.is_some()
            || self.limits.max_tool_invocations.is_some()
            || self.limits.max_llm_tokens.is_some()
            || self.limits.max_wall_clock_secs.is_some()
            || self.limits.max_session_price_usd.is_some()
    }

    pub fn check_pre_llm(&self, root_session_id: &str) -> anyhow::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let mut map = self
            .trees
            .lock()
            .map_err(|e| anyhow::anyhow!("root session budget lock poisoned: {e}"))?;
        let st = map.entry(root_session_id.to_string()).or_default();
        if st.clock_start.is_none() {
            st.clock_start = Some(Instant::now());
        }

        if let Some(max_secs) = self.limits.max_wall_clock_secs {
            if let Some(started) = st.clock_start {
                if started.elapsed().as_secs() >= max_secs {
                    anyhow::bail!(
                        "Root session budget exceeded: wall_clock_secs >= {} (root: {})",
                        max_secs,
                        root_session_id
                    );
                }
            }
        }

        Ok(())
    }

    pub fn reserve_llm_round(&self, root_session_id: &str) -> anyhow::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let Some(max_rounds) = self.limits.max_llm_rounds else {
            return Ok(());
        };
        let mut map = self
            .trees
            .lock()
            .map_err(|e| anyhow::anyhow!("root session budget lock poisoned: {e}"))?;
        let st = map.entry(root_session_id.to_string()).or_default();
        if st.clock_start.is_none() {
            st.clock_start = Some(Instant::now());
        }
        let next = st.llm_rounds.saturating_add(1);
        if next > max_rounds {
            anyhow::bail!(
                "Root session budget exceeded: max_llm_rounds ({}, would be {}) (root: {})",
                max_rounds,
                next,
                root_session_id
            );
        }
        st.llm_rounds = next;
        Ok(())
    }

    pub fn record_llm_completion(
        &self,
        root_session_id: &str,
        input_tokens: u64,
        output_tokens: u64,
        estimated_cost_usd: Option<f64>,
    ) -> anyhow::Result<()> {
        self.record_llm_completion_with_unpriced_override(
            root_session_id,
            input_tokens,
            output_tokens,
            estimated_cost_usd,
            false,
        )
    }

    /// Same as `record_llm_completion`, but allows an explicit capability-gated
    /// override when no catalog price is available.
    pub fn record_llm_completion_with_unpriced_override(
        &self,
        root_session_id: &str,
        input_tokens: u64,
        output_tokens: u64,
        estimated_cost_usd: Option<f64>,
        allow_unpriced_completion: bool,
    ) -> anyhow::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let mut map = self
            .trees
            .lock()
            .map_err(|e| anyhow::anyhow!("root session budget lock poisoned: {e}"))?;
        let st = map.entry(root_session_id.to_string()).or_default();
        if st.clock_start.is_none() {
            st.clock_start = Some(Instant::now());
        }
        let add = input_tokens.saturating_add(output_tokens);
        st.llm_tokens = st.llm_tokens.saturating_add(add);
        if let Some(c) = estimated_cost_usd {
            if c.is_finite() && c >= 0.0 {
                st.session_cost_usd += c;
            }
        } else if let Some(max_price) = self.limits.max_session_price_usd {
            if max_price >= 0.0 {
                if !allow_unpriced_completion {
                    let mode = crate::fail_mode::lookup_fail_mode("P-6.5")
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "refuse-session-start".to_string());
                    anyhow::bail!(
                        "Root session cost-budget enforcement requires price estimation but \
                         catalog is unavailable (P-6.5, I-11: fail-mode={}). \
                         Refusing untracked LLM completion (root: {})",
                        mode,
                        root_session_id
                    );
                }
            }
        }

        if let Some(max_tok) = self.limits.max_llm_tokens {
            if st.llm_tokens > max_tok {
                anyhow::bail!(
                    "Root session budget exceeded: max_llm_tokens ({}, used {}) (root: {})",
                    max_tok,
                    st.llm_tokens,
                    root_session_id
                );
            }
        }

        if let Some(max_price) = self.limits.max_session_price_usd {
            if max_price >= 0.0 && st.session_cost_usd > max_price {
                anyhow::bail!(
                    "Root session budget exceeded: max_session_price_usd ({:.6}, used {:.6}) (root: {})",
                    max_price,
                    st.session_cost_usd,
                    root_session_id
                );
            }
        }

        Ok(())
    }

    pub fn reserve_tool_invocations(
        &self,
        root_session_id: &str,
        count: u64,
    ) -> anyhow::Result<()> {
        if !self.is_enabled() || count == 0 {
            return Ok(());
        }
        let Some(max_tools) = self.limits.max_tool_invocations else {
            return Ok(());
        };
        let mut map = self
            .trees
            .lock()
            .map_err(|e| anyhow::anyhow!("root session budget lock poisoned: {e}"))?;
        let st = map.entry(root_session_id.to_string()).or_default();
        if st.clock_start.is_none() {
            st.clock_start = Some(Instant::now());
        }
        let next = st.tool_invocations.saturating_add(count);
        if next > max_tools {
            anyhow::bail!(
                "Root session budget exceeded: max_tool_invocations ({}, would be {}) (root: {})",
                max_tools,
                next,
                root_session_id
            );
        }
        st.tool_invocations = next;
        Ok(())
    }

    pub fn snapshot_counters(&self, root_session_id: &str) -> Option<(u64, u64, f64)> {
        let map = self.trees.lock().ok()?;
        let st = map.get(root_session_id)?;
        Some((st.llm_rounds, st.llm_tokens, st.session_cost_usd))
    }

    pub fn remove_tree(&self, root_session_id: &str) {
        if let Ok(mut map) = self.trees.lock() {
            map.remove(root_session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_llm_rounds_aggregate_across_sessions() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_llm_rounds: Some(4),
            ..Default::default()
        });
        let root = "root-1";

        reg.reserve_llm_round(root).unwrap();
        reg.record_llm_completion(root, 0, 0, None).unwrap();

        reg.reserve_llm_round(root).unwrap();
        reg.record_llm_completion(root, 0, 0, None).unwrap();

        reg.reserve_llm_round(root).unwrap();
        reg.record_llm_completion(root, 0, 0, None).unwrap();

        reg.reserve_llm_round(root).unwrap();
        reg.record_llm_completion(root, 0, 0, None).unwrap();

        assert!(reg.reserve_llm_round(root).is_err());
    }

    #[test]
    fn tree_tokens_aggregate() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_llm_tokens: Some(200),
            ..Default::default()
        });
        let root = "root-2";

        reg.check_pre_llm(root).unwrap();
        reg.record_llm_completion(root, 80, 70, None).unwrap(); // 150

        assert!(reg.record_llm_completion(root, 60, 50, None).is_err()); // 260 > 200
    }

    #[test]
    fn tree_tool_invocations_reserve() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_tool_invocations: Some(5),
            ..Default::default()
        });
        let root = "root-3";

        reg.reserve_tool_invocations(root, 3).unwrap();
        reg.reserve_tool_invocations(root, 2).unwrap();
        assert!(reg.reserve_tool_invocations(root, 1).is_err());
    }

    #[test]
    fn tree_price_blocks() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_session_price_usd: Some(0.05),
            ..Default::default()
        });
        let root = "root-4";

        reg.check_pre_llm(root).unwrap();
        reg.record_llm_completion(root, 100, 100, Some(0.03))
            .unwrap();
        assert!(reg
            .record_llm_completion(root, 100, 100, Some(0.04))
            .is_err());
    }

    #[test]
    fn remove_tree_clears_counters() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_llm_rounds: Some(1),
            ..Default::default()
        });
        let root = "root-5";

        reg.reserve_llm_round(root).unwrap();
        reg.record_llm_completion(root, 0, 0, None).unwrap();
        assert!(reg.reserve_llm_round(root).is_err());

        reg.remove_tree(root);
        reg.reserve_llm_round(root).unwrap();
    }

    #[test]
    fn independent_trees_dont_interfere() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_llm_rounds: Some(2),
            ..Default::default()
        });

        reg.reserve_llm_round("tree-a").unwrap();
        reg.record_llm_completion("tree-a", 0, 0, None).unwrap();
        reg.reserve_llm_round("tree-a").unwrap();
        reg.record_llm_completion("tree-a", 0, 0, None).unwrap();

        reg.reserve_llm_round("tree-b").unwrap();
        reg.record_llm_completion("tree-b", 0, 0, None).unwrap();

        assert!(reg.reserve_llm_round("tree-a").is_err());
        assert!(reg.reserve_llm_round("tree-b").is_ok());
    }

    #[test]
    fn reserved_round_counts_even_without_completion() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig {
            max_llm_rounds: Some(2),
            ..Default::default()
        });

        reg.reserve_llm_round("root-fail").unwrap();
        reg.reserve_llm_round("root-fail").unwrap();
        assert!(reg.reserve_llm_round("root-fail").is_err());
    }

    #[test]
    fn disabled_when_no_limits() {
        let reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig::default());
        assert!(!reg.is_enabled());
        for _ in 0..1000 {
            reg.check_pre_llm("root").unwrap();
            reg.record_llm_completion("root", u64::MAX, u64::MAX, Some(f64::MAX))
                .unwrap();
            reg.reserve_tool_invocations("root", u64::MAX).unwrap();
        }
    }
}
