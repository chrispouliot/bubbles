use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use rustpush::{APSConnection, APSMessage};

type SubscribeFn = Box<dyn Fn() -> broadcast::Receiver<APSMessage> + Send + Sync>;

/// How many APNs messages the pre-subscribe buffer and the output channel
/// hold. rustpush acknowledges every notification the moment it is read off
/// the socket, so anything that falls out of these buffers is gone for good:
/// Apple will not redeliver an acked message. A day's backlog (messages plus
/// every receipt and typing event for them) arrives as one burst, so this is
/// sized far above any realistic backlog rather than tuned for memory.
pub const APS_BUFFER_CAPACITY: usize = 8192;

struct BufferState<T> {
    buffer: VecDeque<T>,
    subscribed: bool,
}

/// Creates a buffered forwarder over a broadcast source.
///
/// * `source` — the broadcast receiver to read from.
/// * `max_buffer` — max number of pre-subscribe messages to retain; also the
///   capacity of the output channel.
///
/// Returns `(sender, subscribe, dropped)` where:
/// * `sender` — the output broadcast sender (messages are forwarded here once
///   `subscribe` has been called);
/// * `subscribe` — a one-shot closure that drains the internal buffer into a
///   new receiver on `sender` and returns that receiver. After calling it, live
///   messages are forwarded directly.
/// * `dropped` — running count of messages this forwarder had to discard
///   (pre-subscribe overflow, or the source channel lagging). Every one of
///   them was already acknowledged to Apple, so callers should treat a
///   non-zero count as "start a backfill sync".
pub fn make_buffered<T>(
    mut source: broadcast::Receiver<T>,
    max_buffer: usize,
) -> (
    broadcast::Sender<T>,
    impl Fn() -> broadcast::Receiver<T>,
    Arc<AtomicU64>,
)
where
    T: Clone + Send + 'static,
{
    let (output_tx, _) = broadcast::channel(max_buffer.max(1));
    let shared: Arc<Mutex<BufferState<T>>> = Arc::new(Mutex::new(BufferState {
        buffer: VecDeque::new(),
        subscribed: false,
    }));
    let dropped = Arc::new(AtomicU64::new(0));

    let forwarder_shared = shared.clone();
    let forwarder_output = output_tx.clone();
    let forwarder_dropped = Arc::clone(&dropped);
    tokio::spawn(async move {
        loop {
            match source.recv().await {
                Ok(msg) => {
                    let mut state = forwarder_shared.lock().unwrap();
                    if state.subscribed {
                        drop(state);
                        let _ = forwarder_output.send(msg);
                    } else {
                        if state.buffer.len() >= max_buffer {
                            state.buffer.pop_front();
                            let n = forwarder_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                            if n == 1 || n.is_multiple_of(256) {
                                log::error!(
                                    "APNs pre-subscribe buffer full ({max_buffer}); {n} \
                                     already-acknowledged messages dropped so far"
                                );
                            }
                        }
                        state.buffer.push_back(msg);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    forwarder_dropped.fetch_add(n, Ordering::Relaxed);
                    log::error!(
                        "APNs source channel lagged; {n} already-acknowledged messages dropped"
                    );
                }
            }
        }
    });

    let subscribe_output = output_tx.clone();
    let subscribe_fn = move || {
        let rx = subscribe_output.subscribe();
        let mut state = shared.lock().unwrap();
        if !state.subscribed {
            state.subscribed = true;
            while let Some(msg) = state.buffer.pop_front() {
                let _ = subscribe_output.send(msg);
            }
        }
        drop(state);
        rx
    };

    (output_tx, subscribe_fn, dropped)
}

/// A wrapper around `APSConnection` that buffers `messages_cont` messages
/// received before the first subscriber attaches so they are not lost.
pub struct BufferedApsConn {
    inner: APSConnection,
    subscribe_fn: SubscribeFn,
    dropped: Arc<AtomicU64>,
}

impl BufferedApsConn {
    pub fn new(inner: APSConnection) -> Arc<Self> {
        let (_, subscribe_fn, dropped) =
            make_buffered(inner.messages_cont.subscribe(), APS_BUFFER_CAPACITY);
        Arc::new(Self {
            inner,
            subscribe_fn: Box::new(subscribe_fn),
            dropped,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<APSMessage> {
        (self.subscribe_fn)()
    }

    pub fn inner(&self) -> &APSConnection {
        &self.inner
    }

    /// Messages discarded by the buffer so far. Each one was already
    /// acknowledged to Apple and will not be redelivered.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use tokio::sync::broadcast;

    // ------------------------------------------------------------------
    // Pre-subscribe messages are buffered and delivered to the first
    // subscriber when they call the subscribe closure.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn pre_subscribe_messages_are_buffered_and_delivered_to_first_subscriber() {
        let (source_tx, source_rx) = broadcast::channel(16);
        let (_output_tx, subscribe, _dropped) = super::make_buffered(source_rx, 16);

        source_tx.send("msg1".to_string()).unwrap();
        source_tx.send("msg2".to_string()).unwrap();
        source_tx.send("msg3".to_string()).unwrap();

        // Allow the internal forwarding task to drain the source into the buffer.
        tokio::task::yield_now().await;

        let mut rx = subscribe();

        assert_eq!(rx.recv().await.unwrap(), "msg1");
        assert_eq!(rx.recv().await.unwrap(), "msg2");
        assert_eq!(rx.recv().await.unwrap(), "msg3");
    }

    // ------------------------------------------------------------------
    // After subscribe() is called, the buffer is drained / empty.
    // Messages broadcast on the source after the first subscribe are
    // delivered live (not re-sent from the buffer).
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn post_subscribe_messages_are_delivered_live_and_buffer_is_drained() {
        let (source_tx, source_rx) = broadcast::channel(16);
        let (_output_tx, subscribe, _dropped) = super::make_buffered(source_rx, 16);

        // Subscribe first — no pre-subscribe messages; buffer is empty.
        let mut rx = subscribe();

        // Send post-subscribe messages.
        source_tx.send("live1".to_string()).unwrap();
        source_tx.send("live2".to_string()).unwrap();

        // Allow the internal forwarding task to relay them to the output.
        tokio::task::yield_now().await;

        // Both live messages arrive in order.
        assert_eq!(rx.recv().await.unwrap(), "live1");
        assert_eq!(rx.recv().await.unwrap(), "live2");
    }

    // ------------------------------------------------------------------
    // The bounded buffer drops the oldest messages when the cap is
    // exceeded before any subscriber attaches, and counts every drop.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn buffer_cap_drops_oldest_messages_when_exceeded() {
        let (source_tx, source_rx) = broadcast::channel(16);
        let (_output_tx, subscribe, dropped) = super::make_buffered(source_rx, 2);

        source_tx.send("msg1".to_string()).unwrap();
        source_tx.send("msg2".to_string()).unwrap();
        source_tx.send("msg3".to_string()).unwrap();
        source_tx.send("msg4".to_string()).unwrap();
        source_tx.send("msg5".to_string()).unwrap();

        // Allow the internal forwarding task to drain the source.
        tokio::task::yield_now().await;

        let mut rx = subscribe();

        // With max_buffer = 2, only messages 4 and 5 survive;
        // messages 1–3 were dropped.
        let msg_a = rx.recv().await.unwrap();
        let msg_b = rx.recv().await.unwrap();
        assert!(
            msg_a == "msg4" || msg_a == "msg5",
            "expected msg4 or msg5, got {msg_a}"
        );
        assert!(
            msg_b == "msg4" || msg_b == "msg5",
            "expected msg4 or msg5, got {msg_b}"
        );
        assert_ne!(msg_a, msg_b, "must not receive the same message twice");
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            3,
            "every pre-subscribe overflow must be counted so a backfill can be triggered"
        );
    }

    // ------------------------------------------------------------------
    // Nothing dropped => counter stays at zero (the common case must not
    // trigger a spurious backfill).
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn dropped_counter_is_zero_when_buffer_never_overflows() {
        let (source_tx, source_rx) = broadcast::channel(16);
        let (_output_tx, subscribe, dropped) = super::make_buffered(source_rx, 16);

        source_tx.send("msg1".to_string()).unwrap();
        tokio::task::yield_now().await;
        let mut rx = subscribe();
        assert_eq!(rx.recv().await.unwrap(), "msg1");
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    // ------------------------------------------------------------------
    // subscribe() can be called multiple times (not just once).
    // The first call drains the buffer; subsequent calls do NOT re-drain
    // (the buffer is empty after the first call). All subscribers receive
    // live messages.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn subscribe_can_be_called_multiple_times_and_live_messages_delivered_to_all() {
        let (source_tx, source_rx) = broadcast::channel(16);
        let (_output_tx, subscribe, _dropped) = super::make_buffered(source_rx, 16);

        // Pre-subscribe one message — should be buffered.
        source_tx.send("buffered".to_string()).unwrap();
        // Let the forwarder task drain source_rx into the buffer.
        tokio::task::yield_now().await;

        // First subscribe — should drain the buffer.
        let mut rx1 = subscribe();
        assert_eq!(
            rx1.recv().await.unwrap(),
            "buffered",
            "first subscriber should see the buffered message"
        );

        // Second subscribe (after the first) — should NOT re-drain the buffer.
        // The buffer is now empty, so the second subscriber sees only live messages.
        let mut rx2 = subscribe();
        // Send a live message.
        source_tx.send("live".to_string()).unwrap();
        // Let the forwarder task relay it.
        tokio::task::yield_now().await;

        // Both subscribers should see the live message.
        assert_eq!(
            rx1.recv().await.unwrap(),
            "live",
            "first subscriber should see the live message"
        );
        assert_eq!(
            rx2.recv().await.unwrap(),
            "live",
            "second subscriber should see the live message"
        );

        // Third subscribe — should also work and see only future live messages.
        let mut rx3 = subscribe();
        source_tx.send("live2".to_string()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            rx1.recv().await.unwrap(),
            "live2",
            "first subscriber should see live2"
        );
        assert_eq!(
            rx2.recv().await.unwrap(),
            "live2",
            "second subscriber should see live2"
        );
        assert_eq!(
            rx3.recv().await.unwrap(),
            "live2",
            "third subscriber should see live2"
        );
    }
}
