use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time;

use crate::{metrics::Dimension, prometheus::reader::read_prometheus};

pub async fn poll_prometheus(
    poll_endpoint: String,
    interval_seconds: u32,
    bonus_dimensions: HashMap<String, Dimension>,
    table_prefix: String,
) {
    log::info!("polling: {} every: {}s", poll_endpoint, interval_seconds);
    let mut interval = time::interval(time::Duration::from_secs(interval_seconds as u64));
    loop {
        match read_prometheus(
            &poll_endpoint,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_nanos() as u64,
            &bonus_dimensions,
            &table_prefix,
        )
        .await
        {
            Ok(result) => {
                log::info!("here's some stuff: {:?}", result);
            }
            Err(error) => log::error!("error talking to prometheus endpoint: {:?}", error),
        }
        interval.tick().await;
    }
}