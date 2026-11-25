//! Backend asincrono: riceve `Command` dalla UI ed emette `Event`.
//! Tutto l'I/O (rete, file system) avviene qui, mai nel thread di rendering.

use crate::azure::{
    AzureClient, ConnectionInfo, LockedMessage, Message, SubscriptionInfo, entity_path,
};
use crate::config::{self, Config, Profile};
use anyhow::Result;
use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Numero massimo di messaggi scansionati per trovare quello da eliminare.
const DELETE_SCAN_LIMIT: usize = 1000;

#[derive(Debug)]
pub enum Command {
    Connect(Profile),
    SaveConfig(Config),
    LoadTopics,
    LoadSubscriptions {
        topic: String,
    },
    LoadMessages {
        topic: String,
        sub: String,
        dlq: bool,
        max: usize,
    },
    DeleteMessage {
        topic: String,
        sub: String,
        dlq: bool,
        seq: i64,
    },
    Purge {
        topic: String,
        sub: String,
        dlq: bool,
    },
}

#[derive(Debug)]
pub enum Event {
    Connected {
        generation: u64,
        profile: String,
        host: String,
    },
    Progress(String),
    Error {
        messages_op: bool,
        msg: String,
    },
    Topics {
        generation: u64,
        topics: Vec<String>,
    },
    Subscriptions {
        generation: u64,
        topic: String,
        subs: Vec<SubscriptionInfo>,
    },
    Messages {
        generation: u64,
        topic: String,
        sub: String,
        dlq: bool,
        messages: Vec<Message>,
    },
    MessageDeleted {
        generation: u64,
        topic: String,
        sub: String,
        dlq: bool,
        seq: i64,
        found: bool,
    },
    Purged {
        generation: u64,
        topic: String,
        sub: String,
        dlq: bool,
        count: usize,
    },
}

pub async fn run(mut rx: UnboundedReceiver<Command>, tx: UnboundedSender<Event>) {
    let mut client: Option<Arc<AzureClient>> = None;
    let mut generation = 0u64;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Connect(profile) => {
                let res =
                    ConnectionInfo::parse(&profile.connection_string).and_then(AzureClient::new);
                match res {
                    Ok(c) => {
                        generation += 1;
                        let host = c.host().to_string();
                        client = Some(Arc::new(c));
                        let _ = tx.send(Event::Connected {
                            generation,
                            profile: profile.name,
                            host,
                        });
                    }
                    Err(e) => {
                        client = None;
                        let _ = tx.send(Event::Error {
                            messages_op: false,
                            msg: format!("{e:#}"),
                        });
                    }
                }
            }
            Command::SaveConfig(cfg) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = config::save(&cfg).await {
                        let _ = tx.send(Event::Error {
                            messages_op: false,
                            msg: format!("Salvataggio config: {e:#}"),
                        });
                    }
                });
            }
            other => {
                let messages_op = matches!(
                    other,
                    Command::LoadMessages { .. }
                        | Command::DeleteMessage { .. }
                        | Command::Purge { .. }
                );
                let Some(c) = client.clone() else {
                    let _ = tx.send(Event::Error {
                        messages_op,
                        msg: "Nessun profilo attivo".into(),
                    });
                    continue;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(c, generation, other, &tx).await {
                        let _ = tx.send(Event::Error {
                            messages_op,
                            msg: format!("{e:#}"),
                        });
                    }
                });
            }
        }
    }
}

async fn handle(
    c: Arc<AzureClient>,
    generation: u64,
    cmd: Command,
    tx: &UnboundedSender<Event>,
) -> Result<()> {
    match cmd {
        Command::LoadTopics => {
            let topics = c.list_topics().await?;
            let _ = tx.send(Event::Topics { generation, topics });
        }
        Command::LoadSubscriptions { topic } => {
            let subs = c.list_subscriptions(&topic).await?;
            let _ = tx.send(Event::Subscriptions {
                generation,
                topic,
                subs,
            });
        }
        Command::LoadMessages {
            topic,
            sub,
            dlq,
            max,
        } => {
            let path = entity_path(&topic, &sub, dlq);
            // Peek-lock di N messaggi, poi rilascio immediato di tutti i lock.
            let mut locked: Vec<LockedMessage> = Vec::new();
            let mut err = None;
            while locked.len() < max {
                match c.peek_lock(&path).await {
                    Ok(Some(m)) => {
                        locked.push(m);
                        let _ = tx.send(Event::Progress(format!("Peek {}/{max}…", locked.len())));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            unlock_all(&c, &locked).await;
            if let Some(e) = err {
                return Err(e);
            }
            let mut messages: Vec<Message> = locked.into_iter().map(|l| l.message).collect();
            messages.sort_by_key(|m| m.sequence_number);
            let _ = tx.send(Event::Messages {
                generation,
                topic,
                sub,
                dlq,
                messages,
            });
        }
        Command::DeleteMessage {
            topic,
            sub,
            dlq,
            seq,
        } => {
            let path = entity_path(&topic, &sub, dlq);
            let mut locked: Vec<LockedMessage> = Vec::new();
            let mut result: Result<bool> = Ok(false);
            while locked.len() < DELETE_SCAN_LIMIT {
                match c.peek_lock(&path).await {
                    Ok(Some(m)) if m.message.sequence_number == seq => {
                        result = c.delete_locked(&m.location).await.map(|_| true);
                        break;
                    }
                    Ok(Some(m)) => {
                        locked.push(m);
                        let _ = tx.send(Event::Progress(format!(
                            "Ricerca messaggio {seq}… ({})",
                            locked.len()
                        )));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            unlock_all(&c, &locked).await;
            let found = result?;
            let _ = tx.send(Event::MessageDeleted {
                generation,
                topic,
                sub,
                dlq,
                seq,
                found,
            });
        }
        Command::Purge { topic, sub, dlq } => {
            let count = c
                .purge(&topic, &sub, dlq, |count| {
                    let _ = tx.send(Event::Progress(format!(
                        "Purge: {count} messaggi eliminati…"
                    )));
                })
                .await?;
            let _ = tx.send(Event::Purged {
                generation,
                topic,
                sub,
                dlq,
                count,
            });
        }
        Command::Connect(_) | Command::SaveConfig(_) => unreachable!(),
    }
    Ok(())
}

async fn unlock_all(c: &AzureClient, locked: &[LockedMessage]) {
    // Errori ignorati: in ogni caso il lock scade dopo LockDuration.
    join_all(locked.iter().map(|m| c.unlock(&m.location))).await;
}
