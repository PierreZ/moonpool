//! One waker per branch of a future that drives several futures at once.
//!
//! A session drives its reader, its writer and two timers inside one
//! future, and the driver drives its listener, its command queue and every
//! session inside one task. With the task's own waker handed to every
//! branch, any wake re-polled all of them: a frame that arrived re-polled
//! the writer and both timers too, each a lookup in the provider's state.
//! A [`WakeSet`] gives each branch its own waker, which records that branch
//! as ready before it wakes the task, so a pass polls only the branches
//! that were woken.
//!
//! The task is woken exactly when it was before (each branch waker wakes
//! it), so the executor schedules the same tasks in the same order; only
//! the polls that could make no progress go away.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};

/// The task to wake and which branches were woken since the last pass.
struct Shared {
    /// The task's waker, kept after a wake: every branch wake wakes the
    /// task once, as it did when the branch held the task's waker itself.
    /// An executor that schedules by wake (the simulation's seeded
    /// scheduler) then sees the same wakes in the same order.
    task: Mutex<Option<Waker>>,
    ready: AtomicU32,
}

impl Shared {
    fn task(&self) -> std::sync::MutexGuard<'_, Option<Waker>> {
        self.task
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }
}

/// The waker handed to one branch.
struct Branch {
    shared: Arc<Shared>,
    bit: u32,
}

impl Wake for Branch {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.ready.fetch_or(self.bit, Ordering::AcqRel);
        // Wake outside the lock: the executor may poll the task at once.
        let task = self.shared.task().clone();
        if let Some(task) = task {
            task.wake();
        }
    }
}

/// Per-branch wakers for a future that polls up to 32 branches.
pub(crate) struct WakeSet {
    shared: Arc<Shared>,
    wakers: Vec<Waker>,
}

impl WakeSet {
    /// Wakers for `branches` branches, all ready for the first pass.
    pub(crate) fn new(branches: u32) -> Self {
        assert!(
            (1..=32).contains(&branches),
            "a wake set holds 1 to 32 branches, not {branches}"
        );
        let all = u32::MAX >> (32 - branches);
        let shared = Arc::new(Shared {
            task: Mutex::new(None),
            ready: AtomicU32::new(all),
        });
        let wakers = (0..branches)
            .map(|branch| {
                Waker::from(Arc::new(Branch {
                    shared: Arc::clone(&shared),
                    bit: 1 << branch,
                }))
            })
            .collect();
        Self { shared, wakers }
    }

    /// Start a pass: record the task's waker and take the set of branches
    /// woken since the last pass. A wake that lands during the pass marks
    /// its branch for the next one and wakes the task again.
    pub(crate) fn begin(&self, cx: &Context<'_>) -> Ready {
        {
            let mut task = self.shared.task();
            match task.as_ref() {
                Some(current) if current.will_wake(cx.waker()) => {}
                _ => *task = Some(cx.waker().clone()),
            }
        }
        Ready(self.shared.ready.swap(0, Ordering::AcqRel))
    }

    /// The context to poll `branch` with.
    pub(crate) fn context(&self, branch: u32) -> Context<'_> {
        Context::from_waker(&self.wakers[branch as usize])
    }
}

/// The branches woken since the previous pass.
#[derive(Clone, Copy)]
pub(crate) struct Ready(u32);

impl Ready {
    /// Whether `branch` was woken.
    pub(crate) fn has(self, branch: u32) -> bool {
        self.0 & (1 << branch) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_pass_polls_every_branch() {
        let set = WakeSet::new(4);
        let ready = set.begin(&Context::from_waker(Waker::noop()));
        assert!(
            (0..4).all(|branch| ready.has(branch)),
            "all branches start ready"
        );
        let again = set.begin(&Context::from_waker(Waker::noop()));
        assert!(
            (0..4).all(|branch| !again.has(branch)),
            "a pass consumes the readiness"
        );
    }

    #[test]
    fn a_branch_waker_marks_only_its_branch_and_wakes_the_task() {
        struct Count(std::sync::atomic::AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(Count(std::sync::atomic::AtomicUsize::new(0)));
        let task = Waker::from(Arc::clone(&count));
        let set = WakeSet::new(3);
        let _ = set.begin(&Context::from_waker(&task));
        set.context(1).waker().wake_by_ref();
        assert_eq!(count.0.load(Ordering::Relaxed), 1, "the task is woken once");
        let ready = set.begin(&Context::from_waker(&task));
        assert!(
            !ready.has(0) && ready.has(1) && !ready.has(2),
            "only branch 1 is ready"
        );
    }

    #[test]
    fn a_branch_that_was_not_woken_is_not_polled() {
        let set = WakeSet::new(2);
        let mut polls = [0u32; 2];
        let mut pass = |set: &WakeSet| {
            let ready = set.begin(&Context::from_waker(Waker::noop()));
            for (branch, count) in (0u32..).zip(polls.iter_mut()) {
                if ready.has(branch) {
                    *count += 1;
                }
            }
        };
        pass(&set);
        set.context(0).waker().wake_by_ref();
        pass(&set);
        assert_eq!(polls, [2, 1], "branch 1 ran only in the first pass");
    }
}
