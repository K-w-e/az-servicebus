//! AMQP receive-and-delete purge: no HTTP request per message.

use crate::azure::ConnectionInfo;
use anyhow::{Context, Result};
use azservicebus::{
    ServiceBusClient, ServiceBusClientOptions, ServiceBusReceiveMode, ServiceBusReceiver,
    ServiceBusReceiverOptions, ServiceBusTransportType, SubQueue,
};
use futures::{StreamExt, stream::FuturesUnordered};
use std::{future::Future, time::Duration};

const RECEIVER_COUNT: usize = 4;
const BATCH_SIZE: u32 = 100;
const EMPTY_WAIT: Duration = Duration::from_secs(1);

fn receiver_options(dlq: bool) -> ServiceBusReceiverOptions {
    ServiceBusReceiverOptions {
        receive_mode: ServiceBusReceiveMode::ReceiveAndDelete,
        sub_queue: if dlq {
            SubQueue::DeadLetter
        } else {
            SubQueue::None
        },
        // In ReceiveAndDelete mode, prefetch would delete messages before they are counted.
        prefetch_count: 0,
        ..Default::default()
    }
}

// Minimal abstraction for testing the loop without connecting to a real namespace.
trait BatchReceiver {
    fn receive_batch(&mut self) -> impl Future<Output = Result<usize>> + Send;
}

impl BatchReceiver for ServiceBusReceiver {
    async fn receive_batch(&mut self) -> Result<usize> {
        Ok(self
            .receive_messages_with_max_wait_time(BATCH_SIZE, EMPTY_WAIT)
            .await?
            .len())
    }
}

async fn receive_batch<R: BatchReceiver>(receiver: &mut R) -> (&mut R, Result<usize>) {
    let result = receiver.receive_batch().await;
    (receiver, result)
}

async fn drain_batches(
    receivers: &mut [impl BatchReceiver],
    mut progress: impl FnMut(usize),
) -> Result<usize> {
    let mut pending = receivers
        .iter_mut()
        .map(receive_batch)
        .collect::<FuturesUnordered<_>>();
    let mut count = 0;
    let mut error = None;
    while let Some((receiver, result)) = pending.next().await {
        match result {
            Ok(received) => {
                if received > 0 {
                    count += received;
                    progress(count);
                    // A partial batch does NOT mean the queue is empty.
                    if error.is_none() {
                        pending.push(receive_batch(receiver));
                    }
                }
            }
            Err(err) => {
                if error.is_none() {
                    error = Some(err);
                }
                // Do not start more receives, but wait for those already in progress:
                // cancelling them could lose the count of deleted messages.
            }
        }
    }
    match error {
        Some(err) => Err(err).with_context(|| {
            format!("Purge interrupted after {count} confirmed deletions; a failed receive may have deleted additional messages")
        }),
        None => Ok(count),
    }
}

pub async fn purge(
    conn: &ConnectionInfo,
    topic: &str,
    sub: &str,
    dlq: bool,
    progress: impl FnMut(usize) + Send,
) -> Result<usize> {
    let connection_string = format!(
        "Endpoint=sb://{}/;SharedAccessKeyName={};SharedAccessKey={}",
        conn.host, conn.key_name, conn.key
    );
    let mut client = ServiceBusClient::new_from_connection_string(
        connection_string,
        ServiceBusClientOptions {
            // Same port as the HTTPS client; no need to open port 5671.
            transport_type: ServiceBusTransportType::AmqpWebSocket,
            ..Default::default()
        },
    )
    .await
    .context("AMQP connection for purge failed")?;

    let mut receivers = Vec::with_capacity(RECEIVER_COUNT);
    let result: Result<usize> = async {
        // Open all receivers before starting destructive receives.
        for _ in 0..RECEIVER_COUNT {
            receivers.push(
                client
                    .create_receiver_for_subscription(topic, sub, receiver_options(dlq))
                    .await
                    .context("Failed to open AMQP receiver for purge")?,
            );
        }
        drain_batches(&mut receivers, progress).await
    }
    .await;
    // Close all receivers even after a partial open or an error,
    // without hiding the original error. No tasks are left running in the background.
    let receiver_closes =
        futures::future::join_all(receivers.into_iter().map(ServiceBusReceiver::dispose)).await;
    let client_close = client.dispose().await;
    let count = result?;
    for close in receiver_closes {
        close.with_context(|| format!("{count} messages deleted; receiver shutdown failed"))?;
    }
    client_close
        .with_context(|| format!("{count} messages deleted; AMQP client shutdown failed"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeReceiver(VecDeque<Result<usize>>);

    impl BatchReceiver for FakeReceiver {
        async fn receive_batch(&mut self) -> Result<usize> {
            self.0
                .pop_front()
                .expect("Unexpected receive after completion")
        }
    }

    #[test]
    fn destructive_receiver_targets_only_selected_queue_without_prefetch() {
        for dlq in [false, true] {
            let options = receiver_options(dlq);
            assert_eq!(
                options.receive_mode,
                ServiceBusReceiveMode::ReceiveAndDelete
            );
            assert_eq!(options.prefetch_count, 0);
            assert_eq!(
                options.sub_queue,
                if dlq {
                    SubQueue::DeadLetter
                } else {
                    SubQueue::None
                }
            );
        }
    }

    #[tokio::test]
    async fn empty_queue_reports_zero_without_progress() {
        let mut receiver = FakeReceiver([Ok(0)].into());
        let count = drain_batches(std::slice::from_mut(&mut receiver), |_| {
            panic!("Unexpected progress")
        })
        .await
        .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn drains_full_and_partial_batches_until_empty() {
        let mut receiver = FakeReceiver([Ok(100), Ok(3), Ok(100), Ok(0)].into());
        let mut progress = Vec::new();
        let count = drain_batches(std::slice::from_mut(&mut receiver), |n| progress.push(n))
            .await
            .unwrap();
        assert_eq!(count, 203);
        assert_eq!(progress, [100, 103, 203]);
    }

    #[tokio::test]
    async fn aggregates_receivers_with_different_batch_lengths() {
        let mut receivers = [
            FakeReceiver([Ok(100), Ok(3), Ok(0)].into()),
            FakeReceiver([Ok(0)].into()),
            FakeReceiver([Ok(2), Ok(100), Ok(0)].into()),
            FakeReceiver([Ok(7), Ok(0)].into()),
        ];
        let mut progress = Vec::new();
        let count = drain_batches(&mut receivers, |n| progress.push(n))
            .await
            .unwrap();
        assert_eq!(count, 212);
        assert_eq!(progress.len(), 5);
        assert_eq!(progress.last(), Some(&212));
        assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(receivers.iter().all(|receiver| receiver.0.is_empty()));
    }

    struct ConcurrentReceiver {
        barrier: std::sync::Arc<tokio::sync::Barrier>,
        calls: usize,
        fail: bool,
        stop_after_first: bool,
    }

    impl BatchReceiver for ConcurrentReceiver {
        async fn receive_batch(&mut self) -> Result<usize> {
            self.calls += 1;
            if self.calls > 1 {
                assert!(!self.stop_after_first, "Unexpected receive after an error");
                return Ok(0);
            }
            // If receivers ran sequentially, this test would time out.
            self.barrier.wait().await;
            if self.fail {
                anyhow::bail!("network interrupted");
            }
            // Complete after the failed receiver to verify the count.
            tokio::task::yield_now().await;
            Ok(7)
        }
    }

    fn concurrent_receivers(fail: bool) -> Vec<ConcurrentReceiver> {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RECEIVER_COUNT));
        (0..RECEIVER_COUNT)
            .map(|index| ConcurrentReceiver {
                barrier: barrier.clone(),
                calls: 0,
                fail: fail && index < 2,
                stop_after_first: fail,
            })
            .collect()
    }

    #[tokio::test]
    async fn receives_from_all_four_receivers_concurrently() {
        let mut receivers = concurrent_receivers(false);
        let mut progress = Vec::new();
        let count = tokio::time::timeout(
            Duration::from_secs(2),
            drain_batches(&mut receivers, |n| progress.push(n)),
        )
        .await
        .expect("Receives must run concurrently")
        .unwrap();
        assert_eq!(count, 28);
        assert_eq!(progress, [7, 14, 21, 28]);
        assert!(receivers.iter().all(|receiver| receiver.calls == 2));
    }

    #[tokio::test]
    async fn errors_stop_new_receives_but_count_in_flight_successes() {
        let mut receivers = concurrent_receivers(true);
        let mut progress = Vec::new();
        let err = tokio::time::timeout(
            Duration::from_secs(2),
            drain_batches(&mut receivers, |n| progress.push(n)),
        )
        .await
        .expect("In-flight receives must finish")
        .unwrap_err();
        assert_eq!(progress, [7, 14]);
        assert!(receivers.iter().all(|receiver| receiver.calls == 1));
        let message = format!("{err:#}");
        assert!(message.contains("14 confirmed deletions"));
        assert!(message.contains("network interrupted"));
    }

    #[tokio::test]
    async fn error_preserves_partial_count_and_stops() {
        let mut receiver =
            FakeReceiver([Ok(100), Err(anyhow::anyhow!("network interrupted"))].into());
        let mut progress = Vec::new();
        let err = drain_batches(std::slice::from_mut(&mut receiver), |n| progress.push(n))
            .await
            .unwrap_err();
        assert_eq!(progress, [100]);
        let message = format!("{err:#}");
        assert!(message.contains("100 confirmed deletions"));
        assert!(message.contains("network interrupted"));
    }
}
