//! Redis-backed key/value cache used for short-lived session and rate-limit data.

use async_trait::async_trait;
use fred::prelude::*;
use std::time::Duration;

#[async_trait]
pub trait Cache: Send + Sync {
    async fn set(&self, key: &str, value: &str, ttl: Duration) -> anyhow::Result<()>;
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn del(&self, key: &str) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct RedisCache {
    client: Client,
}

impl RedisCache {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let cfg = Config::from_url(url)?;
        let client = Builder::from_config(cfg).build()?;
        client.init().await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn set(&self, key: &str, value: &str, ttl: Duration) -> anyhow::Result<()> {
        self.client
            .set::<(), _, _>(
                key,
                value,
                Some(Expiration::EX(ttl.as_secs() as i64)),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let v: Option<String> = self.client.get(key).await?;
        Ok(v)
    }

    async fn del(&self, key: &str) -> anyhow::Result<()> {
        let _: () = self.client.del(key).await?;
        Ok(())
    }
}
