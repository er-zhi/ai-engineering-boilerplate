// Decides from a tool's risk alone, without an LLM call, whether Execute may run it.

use crate::entity::tool::Risk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Execute,
    RequiresApproval,
}

#[must_use]
pub fn check_policy(risk: Risk) -> PolicyDecision {
    match risk {
        Risk::ReadOnly => PolicyDecision::Execute,
        Risk::Write | Risk::Destructive => PolicyDecision::RequiresApproval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_executes() {
        assert_eq!(check_policy(Risk::ReadOnly), PolicyDecision::Execute);
    }

    #[test]
    fn write_requires_approval() {
        assert_eq!(check_policy(Risk::Write), PolicyDecision::RequiresApproval);
    }

    #[test]
    fn destructive_requires_approval() {
        assert_eq!(
            check_policy(Risk::Destructive),
            PolicyDecision::RequiresApproval
        );
    }
}
