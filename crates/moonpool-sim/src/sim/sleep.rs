//! Cancellation-safe sleep over deterministic simulation time.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    SimulationError, SimulationResult,
    sim::{ScheduleId, WeakSimWorld},
};

/// Future that completes after a specified simulation duration.
///
/// Construction eagerly allocates sequence identity and enqueues the timer,
/// including zero-duration sleeps. Dropping a pending sleep cancels that exact
/// scheduled entry so a losing timeout cannot advance logical time later.
#[derive(Debug)]
pub struct SleepFuture {
    sim: WeakSimWorld,
    state: SleepState,
}

#[derive(Debug)]
enum SleepState {
    Pending {
        task_id: u64,
        schedule_id: ScheduleId,
    },
    Failed(String),
    Complete,
}

impl SleepFuture {
    pub(crate) fn new(sim: WeakSimWorld, task_id: u64, schedule_id: ScheduleId) -> Self {
        Self {
            sim,
            state: SleepState::Pending {
                task_id,
                schedule_id,
            },
        }
    }

    pub(crate) fn failed(sim: WeakSimWorld, message: String) -> Self {
        Self {
            sim,
            state: SleepState::Failed(message),
        }
    }
}

impl Future for SleepFuture {
    type Output = SimulationResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &self.state {
            SleepState::Complete => Poll::Ready(Ok(())),
            SleepState::Failed(message) => {
                Poll::Ready(Err(SimulationError::InvalidState(message.clone())))
            }
            SleepState::Pending { task_id, .. } => {
                let task_id = *task_id;
                let sim = match self.sim.upgrade() {
                    Ok(sim) => sim,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                if sim.poll_sleep(task_id, cx.waker()) {
                    self.state = SleepState::Complete;
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        let SleepState::Pending {
            task_id,
            schedule_id,
        } = self.state
        else {
            return;
        };

        if let Ok(sim) = self.sim.upgrade() {
            sim.cancel_sleep(task_id, schedule_id);
        }
    }
}
