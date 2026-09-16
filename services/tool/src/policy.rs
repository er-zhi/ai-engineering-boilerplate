// Risk policy: deterministic Rust, never an LLM call. ReadOnly runs; Write/Destructive need an
// approval flow Engine doesn't have yet (see the spec's "Три развилки", #2) — Execute reports
// that plainly rather than running the tool or hanging.

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
