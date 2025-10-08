 use anyhow::Result;
 use deadpool_redis::{redis::AsyncCommands, Config, Connection, Pool, Runtime};
 use std::env;

 /// Redis backed cache using Upstash with connection pooling
 pub struct RedisCache {
     pool: Pool,
     ttl_seconds: u64,
     key_prefix: String,
 }

 impl RedisCache {
     const DEFAULT_TTL_SECONDS: u64 = 3600; // 1 hour
     const KEY_PREFIX: &'static str = "letterboxd_cache:";

     pub async fn new() -> Result<Self> {
         let url = env::var("UPSTASH_REDIS_URL")
             .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
         let ttl = env::var("CACHE_TTL_SECONDS")
             .ok()
             .and_then(|s| s.parse().ok())
             .unwrap_or(Self::DEFAULT_TTL_SECONDS);
         let cfg = Config::from_url(url);
         let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
         Ok(RedisCache {
             pool,
             ttl_seconds: ttl,
             key_prefix: Self::KEY_PREFIX.to_string(),
         })
     }

     fn prefixed_key(&self, key: &str) -> String {
         format!("{}{}", self.key_prefix, key)
     }

     pub async fn get(&self, key: &str) -> Result<Option<String>> {
         let mut conn: Connection = self.pool.get().await?;
         let result: Option<String> = conn.get(self.prefixed_key(key)).await?;
         Ok(result)
     }

     pub async fn insert(&self, key: &str, value: &str) -> Result<()> {
         let mut conn: Connection = self.pool.get().await?;
         conn.set_ex::<_, _, ()>(self.prefixed_key(key), value, self.ttl_seconds).await?;
         Ok(())
     }
 }
