// An execution id that has been checked against the caller before anything acts on it.

use uuid::Uuid;

use crate::error::EngineError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedExecution(Uuid);

impl OwnedExecution {
    pub fn checked(
        execution_id: Uuid,
        owner: Option<Uuid>,
        caller: Option<Uuid>,
    ) -> Result<Self, EngineError> {
        if owner.is_some() && owner != caller {
            return Err(EngineError::NotYours(execution_id));
        }
        Ok(Self(execution_id))
    }

    #[must_use]
    pub fn id(self) -> Uuid {
        self.0
    }

    #[cfg(test)]
    pub fn unchecked_for_test(execution_id: Uuid) -> Self {
        Self(execution_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_execution_of_the_caller_is_theirs_to_act_on() {
        let caller = Uuid::new_v4();
        let id = Uuid::new_v4();

        assert_eq!(
            OwnedExecution::checked(id, Some(caller), Some(caller))
                .expect("the caller owns it")
                .id(),
            id
        );
    }

    #[test]
    fn an_execution_of_another_user_cannot_be_built() {
        let refusal =
            OwnedExecution::checked(Uuid::new_v4(), Some(Uuid::new_v4()), Some(Uuid::new_v4()));

        assert!(matches!(refusal, Err(EngineError::NotYours(_))));
    }

    #[test]
    fn an_execution_nobody_owns_is_open_to_anyone() {
        assert!(OwnedExecution::checked(Uuid::new_v4(), None, Some(Uuid::new_v4())).is_ok());
        assert!(OwnedExecution::checked(Uuid::new_v4(), None, None).is_ok());
    }

    #[test]
    fn an_owned_execution_cannot_be_claimed_by_a_caller_with_no_principal() {
        let refusal = OwnedExecution::checked(Uuid::new_v4(), Some(Uuid::new_v4()), None);

        assert!(matches!(refusal, Err(EngineError::NotYours(_))));
    }
}
