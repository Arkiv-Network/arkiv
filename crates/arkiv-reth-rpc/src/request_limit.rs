//! A limit on how many RPC requests run at once, whatever their method.
//!
//! A request that reads the store holds one of its reader slots while it runs.
//! The limit leaves slots free for block building and import, so a burst of
//! requests does not slow them. Further requests wait for a turn.
//!
//! A read keeps running when its caller hangs up. So a request that has its
//! turn runs to its end on a task of its own, and keeps the turn until then.

use std::borrow::Cow;
use std::sync::Arc;

use jsonrpsee::core::middleware::{Batch, BatchEntry, BatchEntryErr, Notification};
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::Request;
use tokio::sync::Semaphore;

/// How many RPC requests run at once unless the node is told otherwise. Each
/// holds one of the store's 112 reader slots at a time, so 48 stay free for
/// block building, import and IPC requests.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;

/// The layer: at most `max` requests run at once, and further ones wait for a
/// turn. reth's own limiter of this kind covers only `debug_` and `trace_`.
#[derive(Clone, Debug)]
pub struct RequestLimit {
    turns: Arc<Semaphore>,
}

impl RequestLimit {
    pub fn new(max: usize) -> Self {
        Self {
            turns: Arc::new(Semaphore::new(max)),
        }
    }
}

impl<S> tower::Layer<S> for RequestLimit {
    type Service = LimitedRequests<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LimitedRequests {
            inner,
            turns: Arc::clone(&self.turns),
        }
    }
}

/// A service whose requests each take a turn.
#[derive(Clone, Debug)]
pub struct LimitedRequests<S> {
    inner: S,
    turns: Arc<Semaphore>,
}

/// Run `request` once a turn is free. A caller that hangs up while it waits
/// takes no turn; one that hangs up later does not stop the request.
async fn in_turn<T: Send + 'static>(
    turns: Arc<Semaphore>,
    request: impl Future<Output = T> + Send + 'static,
) -> T {
    // The semaphore is never closed, so this always gets a turn.
    let turn = turns.acquire_owned().await;
    let request = tokio::spawn(async move {
        let _turn = turn;
        request.await
    });
    match request.await {
        Ok(response) => response,
        Err(failed) if failed.is_panic() => std::panic::resume_unwind(failed.into_panic()),
        // The task was cancelled: the runtime is shutting down, and answers nothing.
        Err(_) => std::future::pending().await,
    }
}

/// `request` with its own copy of what it borrowed, to outlive its caller.
fn owned(request: Request<'_>) -> Request<'static> {
    Request {
        jsonrpc: request.jsonrpc,
        id: request.id.into_owned(),
        method: Cow::Owned(request.method.into_owned()),
        params: request.params.map(|params| Cow::Owned(params.into_owned())),
        extensions: request.extensions,
    }
}

/// `batch` with its own copy of what it borrowed, to outlive its caller.
fn owned_batch(mut batch: Batch<'_>) -> Batch<'static> {
    let extensions = batch.extensions().clone();
    let entries = batch.into_iter().map(|entry| match entry {
        Ok(BatchEntry::Call(request)) => Ok(BatchEntry::Call(owned(request))),
        Ok(BatchEntry::Notification(notification)) => Ok(BatchEntry::Notification(Notification {
            jsonrpc: notification.jsonrpc,
            method: Cow::Owned(notification.method.into_owned()),
            params: notification
                .params
                .map(|params| Cow::Owned(params.into_owned())),
            extensions: notification.extensions,
        })),
        Err(refused) => {
            let (error, id) = refused.into_parts();
            Err(BatchEntryErr::new(id.into_owned(), error.into_owned()))
        }
    });
    let mut owned = Batch::from(entries.collect());
    *owned.extensions_mut() = extensions;
    owned
}

impl<S> RpcServiceT for LimitedRequests<S>
where
    S: RpcServiceT + Send + Sync + Clone + 'static,
    S::MethodResponse: Send + 'static,
    S::BatchResponse: Send + 'static,
{
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;
    type BatchResponse = S::BatchResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let (inner, request) = (self.inner.clone(), owned(request));
        in_turn(
            Arc::clone(&self.turns),
            async move { inner.call(request).await },
        )
    }

    /// A batch takes one turn: its calls run one after another.
    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let (inner, batch) = (self.inner.clone(), owned_batch(batch));
        in_turn(
            Arc::clone(&self.turns),
            async move { inner.batch(batch).await },
        )
    }

    /// A notification runs nothing, so it takes no turn.
    fn notification<'a>(
        &self,
        notification: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(notification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::types::{ErrorObject, Id};
    use serde_json::value::RawValue;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::Layer;

    const LIMIT: usize = 2;
    /// Requests sent at once, more than the limit.
    const REQUESTS: usize = 20;

    /// What a caller puts in a request's extensions.
    #[derive(Clone)]
    struct Marker;

    /// A service that records its requests: how many it ran, how many of them
    /// at once, and each as JSON with whether it carried a [`Marker`].
    #[derive(Clone, Default)]
    struct Counting {
        ran: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl Counting {
        /// One request, run as reth runs a read: on a task of its own, which a
        /// caller that hangs up does not stop.
        async fn run(self) {
            let request = tokio::spawn(async move {
                self.ran.fetch_add(1, Ordering::SeqCst);
                let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.most.fetch_max(running, Ordering::SeqCst);
                tokio::task::yield_now().await;
                self.running.fetch_sub(1, Ordering::SeqCst);
            });
            request.await.unwrap()
        }

        fn saw(&self, json: String, marked: bool) {
            self.seen.lock().unwrap().push((json, marked));
        }
    }

    impl RpcServiceT for Counting {
        type MethodResponse = ();
        type NotificationResponse = ();
        type BatchResponse = ();

        fn call<'a>(&self, request: Request<'a>) -> impl Future<Output = ()> + Send + 'a {
            let marked = request.extensions().get::<Marker>().is_some();
            self.saw(serde_json::to_string(&request).unwrap(), marked);
            self.clone().run()
        }

        fn batch<'a>(&self, mut batch: Batch<'a>) -> impl Future<Output = ()> + Send + 'a {
            let marked = batch.extensions().get::<Marker>().is_some();
            self.saw(batch.to_string(), marked);
            self.clone().run()
        }

        fn notification<'a>(&self, _: Notification<'a>) -> impl Future<Output = ()> + Send + 'a {
            std::future::ready(())
        }
    }

    fn limited() -> (LimitedRequests<Counting>, Counting) {
        let counting = Counting::default();
        (RequestLimit::new(LIMIT).layer(counting.clone()), counting)
    }

    fn call() -> Request<'static> {
        Request::owned("eth_blockNumber".into(), None, Id::Number(1))
    }

    /// A caller that sends `request` and hangs up: polled once, then dropped.
    async fn hang_up(request: impl Future) {
        tokio::select! {
            biased;
            _ = request => {}
            () = std::future::ready(()) => {}
        }
    }

    /// The limit held, and no caller that hung up before its turn ran anything.
    fn assert_limited(counting: &Counting) {
        assert_eq!(counting.most.load(Ordering::SeqCst), LIMIT);
        assert_eq!(counting.ran.load(Ordering::SeqCst), LIMIT + REQUESTS);
    }

    #[tokio::test]
    async fn no_more_calls_run_at_once_than_the_limit() {
        let (service, counting) = limited();
        // The first `LIMIT` of these get a turn, and keep it after they hang up.
        for _ in 0..REQUESTS {
            hang_up(service.call(call())).await;
        }
        let calls: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let service = service.clone();
                tokio::spawn(async move { service.call(call()).await })
            })
            .collect();
        for call in calls {
            call.await.unwrap();
        }
        assert_limited(&counting);
    }

    #[tokio::test]
    async fn a_batch_takes_a_turn_too() {
        let (service, counting) = limited();
        for _ in 0..REQUESTS {
            hang_up(service.batch(Batch::new())).await;
        }
        let batches: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let service = service.clone();
                tokio::spawn(async move { service.batch(Batch::new()).await })
            })
            .collect();
        for batch in batches {
            batch.await.unwrap();
        }
        assert_limited(&counting);
    }

    #[tokio::test]
    async fn a_request_that_panics_panics_its_caller_and_gives_its_turn_back() {
        let turns = Arc::new(Semaphore::new(LIMIT));
        let request = async { panic!("the request panicked") };
        let caller = tokio::spawn(in_turn(Arc::clone(&turns), request));
        assert!(caller.await.unwrap_err().is_panic());
        assert_eq!(turns.available_permits(), LIMIT);
    }

    #[tokio::test]
    async fn a_request_reaches_the_service_as_it_was_sent() {
        let (service, counting) = limited();
        let params = RawValue::from_string(r#"["0x01","latest"]"#.into()).unwrap();

        let mut call = Request::borrowed("eth_getBalance", Some(&params), Id::Str("a".into()));
        let sent_call = serde_json::to_string(&call).unwrap();
        call.extensions_mut().insert(Marker);
        service.call(call.clone()).await;

        call.extensions_mut().clear();
        let note = Notification::new("eth_note".into(), Some(Cow::Borrowed(&*params)));
        let refused = BatchEntryErr::new(Id::Number(7), ErrorObject::borrowed(-32600, "no", None));
        let mut batch = Batch::from(vec![
            Ok(BatchEntry::Call(call)),
            Ok(BatchEntry::Notification(note)),
            Err(refused),
        ]);
        let sent_batch = batch.to_string();
        batch.extensions_mut().insert(Marker);
        service.batch(batch).await;

        let seen = counting.seen.lock().unwrap();
        assert_eq!(*seen, [(sent_call, true), (sent_batch, true)]);
    }
}
