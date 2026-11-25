//! Purge AMQP receive-and-delete: nessuna richiesta HTTP per singolo messaggio.

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
        // In ReceiveAndDelete il prefetch eliminerebbe messaggi non ancora conteggiati.
        prefetch_count: 0,
        ..Default::default()
    }
}

// Astrazione minima per verificare il ciclo senza collegarsi a un namespace reale.
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
                    // Un batch parziale NON implica che la coda sia vuota.
                    if error.is_none() {
                        pending.push(receive_batch(receiver));
                    }
                }
            }
            Err(err) => {
                if error.is_none() {
                    error = Some(err);
                }
                // Non avviare altre ricezioni, ma attendere quelle già in corso:
                // cancellarle potrebbe perdere il conteggio di messaggi eliminati.
            }
        }
    }
    match error {
        Some(err) => Err(err).with_context(|| {
            format!("Purge interrotto dopo {count} messaggi confermati eliminati; una ricezione fallita potrebbe averne eliminati altri")
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
            // Stessa porta del client HTTPS, senza richiedere l'apertura della 5671.
            transport_type: ServiceBusTransportType::AmqpWebSocket,
            ..Default::default()
        },
    )
    .await
    .context("Connessione AMQP per il purge fallita")?;

    let mut receivers = Vec::with_capacity(RECEIVER_COUNT);
    let result: Result<usize> = async {
        // Aprire tutti i receiver prima di iniziare le ricezioni distruttive.
        for _ in 0..RECEIVER_COUNT {
            receivers.push(
                client
                    .create_receiver_for_subscription(topic, sub, receiver_options(dlq))
                    .await
                    .context("Apertura receiver AMQP per il purge fallita")?,
            );
        }
        drain_batches(&mut receivers, progress).await
    }
    .await;
    // Chiudere tutti i receiver anche in caso di apertura parziale o errore,
    // senza nascondere l'errore originale. Nessun task rimane in background.
    let receiver_closes =
        futures::future::join_all(receivers.into_iter().map(ServiceBusReceiver::dispose)).await;
    let client_close = client.dispose().await;
    let count = result?;
    for close in receiver_closes {
        close.with_context(|| format!("{count} messaggi eliminati; chiusura receiver fallita"))?;
    }
    client_close
        .with_context(|| format!("{count} messaggi eliminati; chiusura client AMQP fallita"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeReceiver(VecDeque<Result<usize>>);

    impl BatchReceiver for FakeReceiver {
        async fn receive_batch(&mut self) -> Result<usize> {
            self.0.pop_front().expect("Ricezione inattesa dopo la fine")
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
            panic!("Progresso inatteso")
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
                assert!(!self.stop_after_first, "Nuova ricezione dopo un errore");
                return Ok(0);
            }
            // Se i receiver fossero sequenziali, questo test andrebbe in timeout.
            self.barrier.wait().await;
            if self.fail {
                anyhow::bail!("rete interrotta");
            }
            // Completare dopo il receiver fallito per verificarne il conteggio.
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
        .expect("Le ricezioni devono essere concorrenti")
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
        .expect("Le ricezioni in corso devono terminare")
        .unwrap_err();
        assert_eq!(progress, [7, 14]);
        assert!(receivers.iter().all(|receiver| receiver.calls == 1));
        let message = format!("{err:#}");
        assert!(message.contains("14 messaggi"));
        assert!(message.contains("rete interrotta"));
    }

    #[tokio::test]
    async fn error_preserves_partial_count_and_stops() {
        let mut receiver = FakeReceiver([Ok(100), Err(anyhow::anyhow!("rete interrotta"))].into());
        let mut progress = Vec::new();
        let err = drain_batches(std::slice::from_mut(&mut receiver), |n| progress.push(n))
            .await
            .unwrap_err();
        assert_eq!(progress, [100]);
        let message = format!("{err:#}");
        assert!(message.contains("100 messaggi"));
        assert!(message.contains("rete interrotta"));
    }
}
