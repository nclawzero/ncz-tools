use anyhow::Result;
use std::time::Duration;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
        }
    }
}

/// Retry helper with exponential backoff
pub struct RetryHelper {
    config: RetryConfig,
}

impl RetryHelper {
    /// Create new retry helper with default config
    pub fn new() -> Self {
        Self {
            config: RetryConfig::default(),
        }
    }

    /// Create with custom config
    pub fn with_config(config: RetryConfig) -> Self {
        Self { config }
    }

    /// Execute function with retries
    pub async fn execute<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut delay = self.config.initial_delay;
        let mut attempt = 0;

        loop {
            attempt += 1;

            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if attempt > self.config.max_retries {
                        return Err(e);
                    }

                    eprintln!(
                        "⚠️  Attempt {} failed: {}. Retrying in {:?}...",
                        attempt, e, delay
                    );

                    tokio::time::sleep(delay).await;

                    // Calculate next delay with exponential backoff
                    let next_delay = Duration::from_millis(
                        (delay.as_millis() as f64 * self.config.backoff_multiplier) as u64,
                    );
                    delay = next_delay.min(self.config.max_delay);
                }
            }
        }
    }

    /// Execute with custom max retries
    pub async fn execute_with_retries<F, Fut, T>(&self, retries: u32, f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut config = self.config.clone();
        config.max_retries = retries;

        let helper = RetryHelper::with_config(config);
        helper.execute(f).await
    }
}

impl Default for RetryHelper {
    fn default() -> Self {
        Self::new()
    }
}

/// Categorize error as retryable or not
pub fn is_retryable(error: &str) -> bool {
    let error_lower = error.to_lowercase();

    // Retryable errors
    error_lower.contains("timeout") ||
    error_lower.contains("connection refused") ||
    error_lower.contains("connection reset") ||
    error_lower.contains("network unreachable") ||
    error_lower.contains("temporary failure") ||
    error_lower.contains("429") ||  // Too many requests
    error_lower.contains("503") ||  // Service unavailable
    error_lower.contains("502") // Bad gateway
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_retry_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        let helper = RetryHelper::new();
        let attempt = Arc::new(AtomicU32::new(0));

        let result = helper
            .execute(|| {
                let attempt = Arc::clone(&attempt);
                async move {
                    let n = attempt.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 2 {
                        Err(anyhow::anyhow!("First attempt fails"))
                    } else {
                        Ok("success")
                    }
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(1),
            backoff_multiplier: 2.0,
        };

        let helper = RetryHelper::with_config(config);
        let result = helper
            .execute(|| async { Err::<(), _>(anyhow::anyhow!("Always fails")) })
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_retryable_errors() {
        assert!(is_retryable("connection timeout"));
        assert!(is_retryable("429 Too Many Requests"));
        assert!(is_retryable("503 Service Unavailable"));
        assert!(!is_retryable("401 Unauthorized"));
        assert!(!is_retryable("404 Not Found"));
    }
}
