use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::{HostRequestHandle, TaskHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScheduledTask {
    task: TaskHandle,
    priority: u32,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerCheckpoint {
    Ready { priority: u32, sequence: u64 },
    Waiting { request: HostRequestHandle },
    Detached,
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
    waiting: Vec<(HostRequestHandle, TaskHandle)>,
    sequence: u64,
}

impl Scheduler {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ready: BinaryHeap::with_capacity(capacity),
            waiting: Vec::with_capacity(capacity),
            sequence: 0,
        }
    }

    pub fn schedule(&mut self, task: TaskHandle, priority: u32) {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        self.ready.push(ScheduledTask {
            task,
            priority,
            sequence,
        });
    }

    pub fn deschedule(&mut self, task: TaskHandle) {
        self.ready.retain(|scheduled| scheduled.task != task);
    }

    pub fn wait_for(&mut self, request: HostRequestHandle, task: TaskHandle) {
        if let Some((_, waiting_task)) = self
            .waiting
            .iter_mut()
            .find(|(waiting_request, _)| *waiting_request == request)
        {
            *waiting_task = task;
        } else {
            self.waiting.push((request, task));
        }
    }

    pub fn wake_request(&mut self, request: HostRequestHandle) -> Option<TaskHandle> {
        let position = self
            .waiting
            .iter()
            .position(|(waiting_request, _)| *waiting_request == request)?;
        Some(self.waiting.swap_remove(position).1)
    }

    pub fn cancel_task(&mut self, task: TaskHandle) {
        self.deschedule(task);
        self.waiting
            .retain(|(_, waiting_task)| *waiting_task != task);
    }

    #[must_use]
    pub(crate) fn checkpoint(&self, task: TaskHandle) -> SchedulerCheckpoint {
        if let Some(scheduled) = self.ready.iter().find(|scheduled| scheduled.task == task) {
            return SchedulerCheckpoint::Ready {
                priority: scheduled.priority,
                sequence: scheduled.sequence,
            };
        }
        self.waiting
            .iter()
            .find_map(|(request, waiting)| {
                (*waiting == task).then_some(SchedulerCheckpoint::Waiting { request: *request })
            })
            .unwrap_or(SchedulerCheckpoint::Detached)
    }

    pub(crate) fn restore(&mut self, task: TaskHandle, checkpoint: SchedulerCheckpoint) {
        self.cancel_task(task);
        match checkpoint {
            SchedulerCheckpoint::Ready { priority, sequence } => {
                self.ready.push(ScheduledTask {
                    task,
                    priority,
                    sequence,
                });
                self.sequence = self.sequence.max(sequence.saturating_add(1));
            }
            SchedulerCheckpoint::Waiting { request } => self.wait_for(request, task),
            SchedulerCheckpoint::Detached => {}
        }
    }

    pub fn pop_ready(&mut self) -> Option<TaskHandle> {
        self.ready.pop().map(|scheduled| scheduled.task)
    }

    #[must_use]
    pub(crate) fn reserved_capacities(&self) -> (usize, usize) {
        (self.ready.capacity(), self.waiting.capacity())
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
