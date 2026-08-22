//! Serve a [`tower_service::Service`] through hyper.
//!
//! hyper's [`Service`](hyper::service::Service) takes `&self` and has no
//! readiness step; tower's takes `&mut self` and gates every call on
//! [`poll_ready`](tower_service::Service::poll_ready). [`TowerToHyperService`]
//! reconciles the two by cloning the inner service per request and driving the
//! clone through readiness before calling it, so services that are not always
//! ready (tonic's generated servers, tower middleware stacks) work unchanged.
//!
//! This is a re-implementation of `hyper_util::service::TowerToHyperService`
//! (Apache-2.0/MIT, same licensing family as this crate), kept here so a
//! moonpool stack needs no `hyper-util` dependency: `hyper-util` exists mostly
//! to supply the tokio runtime glue that this crate replaces with the provider
//! traits.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use pin_project_lite::pin_project;

/// A tower service exposed as a [`hyper::service::Service`].
///
/// The inner service is cloned per request, which is the contract tower
/// middleware already expects (clones share state through `Arc` where it
/// matters).
#[derive(Debug, Copy, Clone)]
pub struct TowerToHyperService<S> {
    service: S,
}

impl<S> TowerToHyperService<S> {
    /// Wrap a tower service so hyper can serve it.
    #[must_use]
    pub fn new(service: S) -> Self {
        Self { service }
    }
}

impl<S, R> hyper::service::Service<R> for TowerToHyperService<S>
where
    S: tower_service::Service<R> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TowerToHyperServiceFuture<S, R>;

    fn call(&self, req: R) -> Self::Future {
        TowerToHyperServiceFuture {
            future: Oneshot::NotReady {
                svc: self.service.clone(),
                req: Some(req),
            },
        }
    }
}

pin_project! {
    /// Response future of [`TowerToHyperService`].
    pub struct TowerToHyperServiceFuture<S, R>
    where
        S: tower_service::Service<R>,
    {
        #[pin]
        future: Oneshot<S, R>,
    }
}

impl<S, R> Future for TowerToHyperServiceFuture<S, R>
where
    S: tower_service::Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().future.poll(cx)
    }
}

pin_project! {
    /// Drives one request through a tower service: wait for readiness, call,
    /// then resolve. Owning the service means each request has its own
    /// readiness state, so a service that reports itself busy blocks only the
    /// request holding it.
    #[project = OneshotProj]
    enum Oneshot<S: tower_service::Service<Req>, Req> {
        NotReady {
            svc: S,
            req: Option<Req>,
        },
        Called {
            #[pin]
            fut: S::Future,
        },
        Done,
    }
}

impl<S, Req> Future for Oneshot<S, Req>
where
    S: tower_service::Service<Req>,
{
    type Output = Result<S::Response, S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.as_mut().project() {
                OneshotProj::NotReady { svc, req } => {
                    ready!(svc.poll_ready(cx))?;
                    let fut = svc.call(req.take().expect("request taken only once"));
                    self.set(Oneshot::Called { fut });
                }
                OneshotProj::Called { fut } => {
                    let response = ready!(fut.poll(cx))?;
                    self.set(Oneshot::Done);
                    return Poll::Ready(Ok(response));
                }
                OneshotProj::Done => panic!("polled after completion"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::task::noop_waker_ref;

    use super::{Context, Future, Poll, TowerToHyperService};

    /// A tower service that reports itself busy for the first `busy_polls`
    /// readiness polls, then answers with the request doubled.
    #[derive(Clone)]
    struct SometimesReady {
        busy_polls: usize,
    }

    impl tower_service::Service<u32> for SometimesReady {
        type Response = u32;
        type Error = Infallible;
        type Future = std::future::Ready<Result<u32, Infallible>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            if self.busy_polls == 0 {
                return Poll::Ready(Ok(()));
            }
            self.busy_polls -= 1;
            // Self-wake: nothing external will nudge this service.
            cx.waker().wake_by_ref();
            Poll::Pending
        }

        fn call(&mut self, req: u32) -> Self::Future {
            std::future::ready(Ok(req * 2))
        }
    }

    #[test]
    fn readiness_is_awaited_before_the_call() {
        use hyper::service::Service as _;

        let service = TowerToHyperService::new(SometimesReady { busy_polls: 2 });
        let mut fut = Box::pin(service.call(21));
        let mut cx = Context::from_waker(noop_waker_ref());

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(42))));
    }

    #[test]
    fn a_ready_service_answers_on_the_first_poll() {
        use hyper::service::Service as _;

        let service = TowerToHyperService::new(SometimesReady { busy_polls: 0 });
        let mut fut = Box::pin(service.call(1));
        let mut cx = Context::from_waker(noop_waker_ref());

        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(2))));
    }
}
