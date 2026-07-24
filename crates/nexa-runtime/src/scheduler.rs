use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use crate::{HostRequestHandle, TaskHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScheduledTask {
    task: TaskHandle,
    priority: u32,
    sequence: u64,
}

impl Ord for ScheduledTask {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for ScheduledTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default)]
pub struct Scheduler {
    ready: BinaryHeap<ScheduledTask>,
    waiting: BTreeMap<HostRequestHandle, TaskHandle>,
    sequence: u64,
}

impl Scheduler {
    pub fn schedule(&mut self, task: TaskHandle, priority: u32) {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        self.ready.push(ScheduledTask {
            task,
            priority,
            sequence,
        });
    }

    pub fn wait_for(&mut self, request: HostRequestHandle, task: TaskHandle) {
        self.waiting.insert(request, task);
    }

    pub fn wake_request(
        &mut self,
        request: HostRequestHandle,
        priority: u32,
    ) -> Option<TaskHandle> {
        let task = self.waiting.remove(&request)?;
        self.schedule(task, priority);
        Some(task)
    }

    pub fn pop_ready(&mut self) -> Option<TaskHandle> {
        self.ready.pop().map(|scheduled| scheduled.task)
    }

    #[must_use]
    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    #[must_use]
    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::RawHandle;

    use super::Scheduler;
    use crate::TaskHandle;

    #[test]
    fn priority_then_fifo_order_is_stable() {
        let task = |index| TaskHandle::from_raw(RawHandle::new(1, index, 0));
        let mut scheduler = Scheduler::default();
        scheduler.schedule(task(0), 1);
        scheduler.schedule(task(1), 2);
        scheduler.schedule(task(2), 2);
        assert_eq!(scheduler.pop_ready(), Some(task(1)));
        assert_eq!(scheduler.pop_ready(), Some(task(2)));
        assert_eq!(scheduler.pop_ready(), Some(task(0)));
    }
}
