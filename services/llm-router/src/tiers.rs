// Deployment config: which models serve each tier. The limits callers must respect live in llm.rs.

use common::proto::llm_router::v1::QualityTier;

#[derive(Debug)]
pub struct TierModels {
    pub primary: String,
    pub backup: String,
}

#[derive(Debug)]
pub struct Tiers {
    low: TierModels,
    medium: TierModels,
    high: TierModels,
}

impl Tiers {
    pub fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Ok(Self {
            low: TierModels::from_vars(&lookup, "LOW")?,
            medium: TierModels::from_vars(&lookup, "MEDIUM")?,
            high: TierModels::from_vars(&lookup, "HIGH")?,
        })
    }

    pub fn models(&self, tier: QualityTier) -> Option<&TierModels> {
        match tier {
            QualityTier::Low => Some(&self.low),
            QualityTier::Medium => Some(&self.medium),
            QualityTier::High => Some(&self.high),
            QualityTier::Unspecified => None,
        }
    }
}

impl TierModels {
    fn from_vars(lookup: &impl Fn(&str) -> Option<String>, tier: &str) -> Result<Self, String> {
        Ok(Self {
            primary: required(lookup, &format!("LLM_{tier}_PRIMARY"))?,
            backup: required(lookup, &format!("LLM_{tier}_BACKUP"))?,
        })
    }
}

fn required(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    lookup(name)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(name: &str) -> Option<String> {
        match name {
            "LLM_LOW_PRIMARY" => Some("deepseek/deepseek-v4-flash".to_owned()),
            "LLM_LOW_BACKUP" => Some("z-ai/glm-4.7-flash".to_owned()),
            "LLM_MEDIUM_PRIMARY" => Some("deepseek/deepseek-v3.2".to_owned()),
            "LLM_MEDIUM_BACKUP" => Some("z-ai/glm-4.7".to_owned()),
            "LLM_HIGH_PRIMARY" => Some("z-ai/glm-5".to_owned()),
            "LLM_HIGH_BACKUP" => Some("deepseek/deepseek-r1-0528".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn every_tier_gets_its_primary_and_backup() {
        let tiers = Tiers::from_vars(configured).unwrap();

        let low = tiers.models(QualityTier::Low).unwrap();
        assert_eq!(low.primary, "deepseek/deepseek-v4-flash");
        assert_eq!(low.backup, "z-ai/glm-4.7-flash");
        assert_eq!(
            tiers.models(QualityTier::High).unwrap().primary,
            "z-ai/glm-5"
        );
    }

    #[test]
    fn a_tier_without_models_is_not_served() {
        let tiers = Tiers::from_vars(configured).unwrap();

        assert!(tiers.models(QualityTier::Unspecified).is_none());
    }

    #[test]
    fn missing_variable_is_named_in_the_error() {
        let without_high_backup = |name: &str| match name {
            "LLM_HIGH_BACKUP" => None,
            other => configured(other),
        };

        let error = Tiers::from_vars(without_high_backup).unwrap_err();

        assert!(error.contains("LLM_HIGH_BACKUP"), "{error}");
    }
}
