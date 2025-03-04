use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use tokio::time::sleep;

use collector_client::{CollectPackagesRequest, CollectPackagesResponse, CollectorClient};

use crate::config::{CollectorConfig, Interest};
use crate::state::AppState;

#[derive(Debug, thiserror::Error)]
#[error("No configuration for collector")]
pub struct NoCollectorConfigError;

#[derive(Copy, Clone)]
pub enum RetentionMode {
    All,
    InterestingOnly,
}

pub struct Collector {
    pub(crate) id: String,
    pub(crate) config: CollectorConfig,
    pub(crate) client: Arc<CollectorClient>,
}

impl Collector {
    pub async fn collect_packages(
        &self,
        state: &AppState,
        purls: Vec<String>,
    ) -> Result<CollectPackagesResponse, anyhow::Error> {
        Self::collect_packages_internal(
            &self.client,
            state,
            self.id.clone(),
            purls,
            self.config.cadence,
            RetentionMode::InterestingOnly,
        )
        .await
    }

    async fn collect_packages_internal(
        client: &CollectorClient,
        state: &AppState,
        id: String,
        purls: Vec<String>,
        cadence: Duration,
        mode: RetentionMode,
    ) -> Result<CollectPackagesResponse, anyhow::Error> {
        //log::info!("{} scan {:?}", id, purls);

        let purls = state.db.filter_purls_as_of(&id, purls, Utc::now() - cadence).await?;

        let response = client
            .collect_packages(CollectPackagesRequest { purls: purls.clone() })
            .await;

        match response {
            Ok(response) => {
                for purl in response.purls.keys() {
                    log::info!("[{id}] scanned {} {:?}", purl, response.purls.get(purl));
                    let _ = state.db.insert_purl(purl).await.ok();
                    let _ = state.db.update_purl_scan_time(&id, purl).await.ok();
                }

                if matches!(mode, RetentionMode::All) {
                    for purl in &purls {
                        let _ = state.db.update_purl_scan_time(&id, purl).await.ok();
                    }
                }

                Ok(response)
            }
            Err(e) => {
                log::warn!("[{id}] collector response: {}", e);
                Err(e)
            }
        }
    }

    pub async fn update(client: Arc<CollectorClient>, state: Arc<AppState>, id: String) {
        loop {
            if let Some(config) = state.collectors.collector_config(id.clone()) {
                let collector_url = config.url.clone();
                if config.interests.contains(&Interest::Package) {
                    let purls: Vec<String> = state
                        .db
                        .get_purls_to_scan(id.as_str(), Utc::now() - config.cadence, 20)
                        .await
                        .collect()
                        .await;

                    if !purls.is_empty() {
                        log::debug!("polling packages for {} -> {}", id, collector_url);
                        if let Ok(response) = Self::collect_packages_internal(
                            &client,
                            &state,
                            id.clone(),
                            purls,
                            config.cadence,
                            RetentionMode::All,
                        )
                        .await
                        {
                            // during normal re-scan, we did indeed discover some vulns, make sure they are in the DB.
                            let vuln_ids: HashSet<_> = response.purls.values().flatten().collect();

                            for vuln_id in vuln_ids {
                                state.db.insert_vulnerability(vuln_id).await.ok();
                            }
                        }
                    }
                }
            }
            // TODO: configurable or smarter for rate-limiting
            sleep(Duration::from_secs(1)).await;
        }
    }
}
