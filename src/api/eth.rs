use crate::{
    api::{BaseApi, ValueHandle},
    client::Client,
};
use jsonrpsee::core::JsonValue;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

pub struct EthApi {
    inner: BaseApi,
    stale_timeout: Duration,
}

impl EthApi {
    pub fn new(client: Arc<Client>, stale_timeout: Duration) -> Self {
        let (head_tx, head_rx) = watch::channel::<Option<(JsonValue, u64)>>(None);
        let (finalized_head_tx, finalized_head_rx) =
            watch::channel::<Option<(JsonValue, u64)>>(None);

        let this = Self {
            inner: BaseApi::new(client, head_rx, finalized_head_rx),
            stale_timeout,
        };

        this.start_background_task(head_tx, finalized_head_tx);

        this
    }

    pub fn get_head(&self) -> ValueHandle<(JsonValue, u64)> {
        self.inner.get_head()
    }

    pub fn get_finalized_head(&self) -> ValueHandle<(JsonValue, u64)> {
        self.inner.get_finalized_head()
    }

    pub fn current_head(&self) -> Option<(JsonValue, u64)> {
        self.inner.head_rx.borrow().to_owned()
    }

    pub fn current_finalized_head(&self) -> Option<(JsonValue, u64)> {
        self.inner.finalized_head_rx.borrow().to_owned()
    }

    fn start_background_task(
        &self,
        head_tx: watch::Sender<Option<(JsonValue, u64)>>,
        finalized_head_tx: watch::Sender<Option<(JsonValue, u64)>>,
    ) {
        let client = self.inner.client.clone();
        let stale_timeout = self.stale_timeout;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(stale_timeout);

            loop {
                let client = client.clone();

                let run = async {
                    interval.reset();

                    // query current head
                    let head = client
                        .request("eth_getBlockByNumber", vec!["latest".into(), true.into()])
                        .await?;
                    let number = super::get_number(&head)?;
                    let hash = super::get_hash(&head)?;

                    tracing::debug!("New head: {number} {hash}");
                    head_tx.send_replace(Some((hash, number)));

                    let mut sub = client
                        .subscribe(
                            "eth_subscribe",
                            ["newHeads".into()].into(),
                            "eth_unsubscribe",
                        )
                        .await?;

                    loop {
                        tokio::select! {
                            val = sub.next() => {
                                if let Some(Ok(val)) = val {
                                    interval.reset();

                                    let number = super::get_number(&val)?;
                                    let hash = super::get_hash(&val)?;

                                    tracing::debug!("New head: {number} {hash}");
                                    head_tx.send_replace(Some((hash, number)));
                                } else {
                                    break;
                                }
                            }
                            _ = interval.tick() => {
                                tracing::warn!("No new blocks for {stale_timeout:?} seconds, rotating endpoint");
                                client.rotate_endpoint().await.expect("Failed to rotate endpoint");
                                break;
                            }
                        }
                    }

                    Ok::<(), anyhow::Error>(())
                };

                if let Err(e) = run.await {
                    tracing::error!("Error in background task: {e}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        let client = self.inner.client.clone();
        tokio::spawn(async move {
            loop {
                let run = async {
                    let mut sub = client
                        .subscribe(
                            "eth_subscribe",
                            ["newFinalizedHeads".into()].into(),
                            "eth_unsubscribe",
                        )
                        .await?;

                    while let Some(Ok(val)) = sub.next().await {
                        let number = super::get_number(&val)?;
                        let hash = super::get_hash(&val)?;

                        tracing::debug!("New finalized head: {number} {hash}");
                        finalized_head_tx.send_replace(Some((hash, number)));
                    }

                    Ok::<(), anyhow::Error>(())
                };

                if let Err(e) = run.await {
                    // cannot figure out finalized head
                    finalized_head_tx.send_replace(None);
                    tracing::error!("Error in background task: {e}");
                    if e.to_string().contains("invalid") {
                        // finalized head subscription is not supported
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}
