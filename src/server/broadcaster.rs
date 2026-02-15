use std::sync::Arc;
use tokio::sync::broadcast;

/// The data type broadcast to all subscribers.
pub type BroadcastData = Arc<Vec<u8>>;
pub type BroadcastSender = broadcast::Sender<BroadcastData>;
pub type BroadcastReceiver = broadcast::Receiver<BroadcastData>;

/// Capacity for the broadcast channel. Slow consumers that fall behind
/// this many messages will get a `Lagged` error.
const BROADCAST_CAPACITY: usize = 256;

/// Creates a new broadcast channel for serial data fan-out.
///
/// Returns (sender, receiver). Additional receivers are created via `sender.subscribe()`.
/// Data is wrapped in `Arc<Vec<u8>>` to avoid cloning the payload per subscriber.
pub fn new_broadcast() -> (BroadcastSender, BroadcastReceiver) {
    broadcast::channel(BROADCAST_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_subscriber_receives() {
        let (tx, mut rx) = new_broadcast();
        let data = Arc::new(vec![1, 2, 3]);
        tx.send(data.clone()).unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(*received, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_data() {
        let (tx, mut rx1) = new_broadcast();
        let mut rx2 = tx.subscribe();
        let mut rx3 = tx.subscribe();

        let data = Arc::new(b"hello".to_vec());
        tx.send(data.clone()).unwrap();

        let r1 = rx1.recv().await.unwrap();
        let r2 = rx2.recv().await.unwrap();
        let r3 = rx3.recv().await.unwrap();

        assert_eq!(*r1, *data);
        assert_eq!(*r2, *data);
        assert_eq!(*r3, *data);
    }

    #[tokio::test]
    async fn lagged_subscriber() {
        let (tx, _rx) = new_broadcast();
        let mut slow_rx = tx.subscribe();

        // Fill the channel beyond capacity
        for i in 0..BROADCAST_CAPACITY + 10 {
            #[allow(clippy::cast_possible_truncation)]
            let _ = tx.send(Arc::new(vec![i as u8]));
        }

        // The slow subscriber should get a Lagged error
        let result = slow_rx.recv().await;
        assert!(matches!(result, Err(broadcast::error::RecvError::Lagged(_))));
    }

    #[tokio::test]
    async fn no_subscribers_send_does_not_panic() {
        let (tx, rx) = new_broadcast();
        drop(rx);
        // Send should return an error (no receivers) but not panic
        let result = tx.send(Arc::new(vec![1]));
        assert!(result.is_err());
    }
}
