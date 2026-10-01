use crate::Signal;
use crate::call_state::TelephonyAction;

/// Every current transition emits at most four ordered actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actions {
    items: [TelephonyAction; 4],
    length: usize,
}
impl Actions {
    pub(crate) fn new() -> Self {
        Self {
            items: [const { TelephonyAction::IgnoreSignal(Signal::Raw(0)) }; 4],
            length: 0,
        }
    }
    pub(crate) fn from_array<const N: usize>(items: [TelephonyAction; N]) -> Self {
        let mut actions = Self::new();
        for item in items {
            actions.push(item);
        }
        actions
    }
    pub(crate) fn push(&mut self, action: TelephonyAction) {
        assert!(
            self.length < self.items.len(),
            "transition action capacity exceeded"
        );
        self.items[self.length] = action;
        self.length += 1;
    }
    pub fn as_slice(&self) -> &[TelephonyAction] {
        &self.items[..self.length]
    }
}
