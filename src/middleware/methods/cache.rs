use async_trait::async_trait;
use blake2::Blake2b512;
use jsonrpsee::{core::JsonValue, types::ErrorObjectOwned};
use opentelemetry::trace::FutureExt;

use crate::{
    cache::{Cache, CacheKey},
    helpers,
    middleware::{call::CallRequest, Middleware, NextFn},
};

pub struct CacheMiddleware {
    cache: Cache<Blake2b512>,
}

impl CacheMiddleware {
    pub fn new(cache: Cache<Blake2b512>) -> Self {
        Self { cache }
    }
}

const TRACER: helpers::telemetry::Tracer = helpers::telemetry::Tracer::new("cache-middleware");

#[async_trait]
impl Middleware<CallRequest, Result<JsonValue, ErrorObjectOwned>> for CacheMiddleware {
    async fn call(
        &self,
        request: CallRequest,
        next: NextFn<CallRequest, Result<JsonValue, ErrorObjectOwned>>,
    ) -> Result<JsonValue, ErrorObjectOwned> {
        async move {
            if request.extra.bypass_cache {
                return next(request).await;
            }

            let key = CacheKey::<Blake2b512>::new(&request.method, &request.params);

            if let Some(value) = self.cache.get(&key) {
                return Ok(value);
            }

            let result = next(request).await;

            if let Ok(ref value) = result {
                // avoid caching null value because it usually means data not available
                // but it could be available in the future
                if !value.is_null() {
                    let cache = self.cache.clone();
                    let value = value.clone();
                    tokio::spawn(async move {
                        cache.insert(key, value).await;
                    });
                }
            }

            result
        }
        .with_context(TRACER.context("call"))
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::new_cache;
    use futures::FutureExt;
    use serde_json::json;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn handle_ok_resp() {
        let middleware = CacheMiddleware::new(Cache::new(3));

        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { Ok(json!(1)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(1));

        // wait for cache write
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        // cache hit
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { panic!() }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(1));

        // cache miss with different params
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(22)]),
                Box::new(move |_| async move { Ok(json!(2)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(2));

        // cache miss with different method
        let res = middleware
            .call(
                CallRequest::new("test2", vec![json!(22)]),
                Box::new(move |_| async move { Ok(json!(3)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(3));

        // cache hit and update prune priority
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { panic!() }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(1));

        // cache override oldest entry
        let res = middleware
            .call(
                CallRequest::new("test2", vec![json!(33)]),
                Box::new(move |_| async move { Ok(json!(4)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(4));

        // cache miss due to entry pruned
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(22)]),
                Box::new(move |_| async move { Ok(json!(5)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(5));
    }

    #[tokio::test]
    async fn should_not_cache_null() {
        let middleware = CacheMiddleware::new(Cache::new(3));

        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { Ok(JsonValue::Null) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), JsonValue::Null);

        // wait for cache write
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        // should not be cached
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { Ok(json!(2)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(2));
    }

    #[tokio::test]
    async fn cache_ttl_works() {
        let middleware = CacheMiddleware::new(new_cache(
            NonZeroUsize::new(1).unwrap(),
            Some(Duration::from_millis(10)),
        ));

        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { Ok(json!(1)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(1));

        // wait for cache write
        tokio::time::sleep(Duration::from_millis(1)).await;

        // cache hit
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { panic!() }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(1));

        // wait for cache to expire
        tokio::time::sleep(Duration::from_millis(10)).await;

        // cache miss
        let res = middleware
            .call(
                CallRequest::new("test", vec![json!(11)]),
                Box::new(move |_| async move { Ok(json!(2)) }.boxed()),
            )
            .await;
        assert_eq!(res.unwrap(), json!(2));
    }
}
