/// Pure state machine. The native adapters supply USB presence snapshots after
/// debounce; startup absence and sleep teardown are not handoffs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    ClaimLocal,
    HandoffRemote,
}

#[derive(Debug)]
pub struct Controller {
    present: bool,
    suspended: bool,
}
impl Controller {
    pub fn new(present: bool) -> Self {
        Self {
            present,
            suspended: false,
        }
    }
    pub fn present(&self) -> bool {
        self.present
    }
    pub fn observe(&mut self, present: bool) -> Option<Transition> {
        if self.suspended || self.present == present {
            return None;
        }
        self.present = present;
        Some(if present {
            Transition::ClaimLocal
        } else {
            Transition::HandoffRemote
        })
    }
    pub fn suspend(&mut self) {
        self.suspended = true;
    }
    pub fn resume(&mut self, present: bool) -> Option<Transition> {
        self.suspended = false;
        self.present = present;
        present.then_some(Transition::ClaimLocal)
    }
}
