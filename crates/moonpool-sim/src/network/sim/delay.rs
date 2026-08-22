//! Cancellation-safe delayed network operation.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{ScheduleId, SimulationResult, WeakSimWorld};

use super::event::NetworkOperationId;

/// Future completed by a targeted network operation event.
#[derive(Debug)]
pub(crate) struct NetworkDelay {
    sim: WeakSimWorld,
    operation_id: NetworkOperationId,
    schedule_id: ScheduleId,
    completed: bool,
}

impl NetworkDelay {
    pub(crate) fn new(
        sim: WeakSimWorld,
        operation_id: NetworkOperationId,
        schedule_id: ScheduleId,
    ) -> Self {
        Self {
            sim,
            operation_id,
            schedule_id,
            completed: false,
        }
    }
}

impl Future for NetworkDelay {
    type Output = SimulationResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sim = match self.sim.upgrade() {
            Ok(sim) => sim,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match sim.poll_network_operation(self.operation_id, cx.waker()) {
            Ok(true) => {
                self.completed = true;
                Poll::Ready(Ok(()))
            }
            Ok(false) => Poll::Pending,
            Err(error) => {
                self.completed = true;
                Poll::Ready(Err(error))
            }
        }
    }
}

impl Drop for NetworkDelay {
    fn drop(&mut self) {
        if !self.completed
            && let Ok(sim) = self.sim.upgrade()
        {
            sim.cancel_network_operation(self.operation_id, self.schedule_id);
        }
    }
}
