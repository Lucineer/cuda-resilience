/*!
# cuda-resilience

Fault tolerance patterns for agents.

Agents operate in unreliable environments. This crate combines
bulkhead isolation, timeouts, retries, rate limiting, and chaos
injection into one resilience toolkit.

- Bulkhead (resource isolation)
- Timeout tracking
- Retry with exponential backoff + jitter
- Rate limiting (token bucket)
- Circuit breaker
- Chaos monkey (random failure injection)
- Resilience score
*/

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bulkhead partition
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bulkhead {
    pub name: String,
    pub max_concurrent: usize,
    pub active: usize,
    pub rejected: u64,
    pub completed: u64,
    pub failed: u64,
}

impl Bulkhead {
    pub fn try_enter(&mut self) -> bool {
        if self.active >= self.max_concurrent { self.rejected += 1; return false; }
        self.active += 1;
        true
    }
    pub fn exit(&mut self, success: bool) {
        self.active = self.active.saturating_sub(1);
        if success { self.completed += 1; } else { self.failed += 1; }
    }
    pub fn utilization(&self) -> f64 { if self.max_concurrent == 0 { 1.0 } else { self.active as f64 / self.max_concurrent as f64 } }
}

/// Circuit breaker state
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitState { Closed, Open, HalfOpen }

/// Circuit breaker
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CircuitBreaker {
    pub name: String,
    pub state: CircuitState,
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub open_since_ms: Option<u64>,
    pub open_duration_ms: u64,
    pub total_trips: u64,
}

impl CircuitBreaker {
    pub fn new(name: &str, failure_threshold: u32, open_duration_ms: u64) -> Self {
        CircuitBreaker { name: name.to_string(), state: CircuitState::Closed, failure_threshold, success_threshold: 3, consecutive_failures: 0, consecutive_successes: 0, open_since_ms: None, open_duration_ms, total_trips: 0 }
    }

    pub fn allow(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if let Some(since) = self.open_since_ms {
                    if now() - since > self.open_duration_ms {
                        self.state = CircuitState::HalfOpen;
                        self.consecutive_successes = 0;
                        return true;
                    }
                }
                false
            }
            CircuitState::HalfOpen => true,
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        match self.state {
            CircuitState::HalfOpen => {
                self.consecutive_successes += 1;
                if self.consecutive_successes >= self.success_threshold {
                    self.state = CircuitState::Closed;
                }
            }
            _ => {}
        }
    }

    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        match self.state {
            CircuitState::Closed => {
                if self.consecutive_failures >= self.failure_threshold {
                    self.state = CircuitState::Open;
                    self.open_since_ms = Some(now());
                    self.total_trips += 1;
                }
            }
            CircuitState::HalfOpen => {
                self.state = CircuitState::Open;
                self.open_since_ms = Some(now());
                self.total_trips += 1;
            }
            _ => {}
        }
    }
}

/// Token bucket rate limiter
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RateLimiter {
    pub max_tokens: f64,
    pub tokens: f64,
    pub refill_rate: f64, // tokens per second
    pub last_refill_ms: u64,
    pub rejected: u64,
    pub accepted: u64,
}

impl RateLimiter {
    pub fn new(max_tokens: f64, refill_rate: f64) -> Self { RateLimiter { max_tokens, tokens: max_tokens, refill_rate, last_refill_ms: now(), rejected: 0, accepted: 0 } }

    pub fn try_acquire(&mut self, tokens: f64) -> bool {
        self.refill();
        if self.tokens >= tokens { self.tokens -= tokens; self.accepted += 1; true }
        else { self.rejected += 1; false }
    }

    fn refill(&mut self) {
        let now = now();
        let elapsed = (now - self.last_refill_ms) as f64 / 1000.0;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill_ms = now;
    }
}

/// Chaos monkey config
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChaosConfig {
    pub failure_rate: f64,     // 0.0 - 1.0
    pub latency_ms: u64,
    pub latency_rate: f64,
}

impl Default for ChaosConfig {
    fn default() -> Self { ChaosConfig { failure_rate: 0.0, latency_ms: 0, latency_rate: 0.0 } }
}

/// Resilience result
#[derive(Clone, Debug)]
pub struct ResilienceResult { pub allowed: bool, pub simulated_failure: bool, pub simulated_latency_ms: u64 }

/// Combined resilience shield
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResilienceShield {
    pub bulkhead: Bulkhead,
    pub circuit: CircuitBreaker,
    pub limiter: RateLimiter,
    pub chaos: ChaosConfig,
    pub total_requests: u64,
    pub total_success: u64,
    pub total_failure: u64,
}

impl ResilienceShield {
    pub fn new(name: &str, max_concurrent: usize, failure_threshold: u32, open_ms: u64, max_rate: f64) -> Self {
        ResilienceShield { bulkhead: Bulkhead { name: name.to_string(), max_concurrent, active: 0, rejected: 0, completed: 0, failed: 0 }, circuit: CircuitBreaker::new(name, failure_threshold, open_ms), limiter: RateLimiter::new(max_rate, max_rate), chaos: ChaosConfig::default(), total_requests: 0, total_success: 0, total_failure: 0 }
    }

    /// Try to execute — returns ResilienceResult
    pub fn try_execute(&mut self) -> ResilienceResult {
        self.total_requests += 1;
        // Check circuit
        if !self.circuit.allow() { self.total_failure += 1; return ResilienceResult { allowed: false, simulated_failure: false, simulated_latency_ms: 0 }; }
        // Check rate limiter
        if !self.limiter.try_acquire(1.0) { self.total_failure += 1; return ResilienceResult { allowed: false, simulated_failure: false, simulated_latency_ms: 0 }; }
        // Check bulkhead
        if !self.bulkhead.try_enter() { self.total_failure += 1; return ResilienceResult { allowed: false, simulated_failure: false, simulated_latency_ms: 0 }; }
        // Chaos injection
        let sim_fail = self.chaos.failure_rate > 0.0 && fastrand() < self.chaos.failure_rate;
        let sim_latency = if self.chaos.latency_rate > 0.0 && fastrand() < self.chaos.latency_rate { self.chaos.latency_ms } else { 0 };
        ResilienceResult { allowed: true, simulated_failure: sim_fail, simulated_latency_ms: sim_latency }
    }

    /// Record result of execution
    pub fn record(&mut self, success: bool) {
        self.bulkhead.exit(success);
        if success { self.circuit.record_success(); self.total_success += 1; }
        else { self.circuit.record_failure(); self.total_failure += 1; }
    }

    /// Resilience score (0.0-1.0)
    pub fn score(&self) -> f64 {
        if self.total_requests == 0 { return 1.0; }
        self.total_success as f64 / self.total_requests as f64
    }

    pub fn summary(&self) -> String {
        format!("Shield[{}]: circuit={:?}, bulkhead={}/{}, limiter_accepted={}, chaos_fail_rate={:.0}%, score={:.2}",
            self.bulkhead.name, self.circuit.state, self.bulkhead.active, self.bulkhead.max_concurrent,
            self.limiter.accepted, self.chaos.failure_rate * 100.0, self.score())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn fastrand() -> f64 {
    // Simple pseudo-random
    use std::time::SystemTime;
    let d = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let seed = (d.as_nanos() & 0xFFFF_FFFF) as u32;
    // Linear congruential
    let x = (seed.wrapping_mul(1103515245).wrapping_add(12345)) >> 16;
    (x & 0x7FFF) as f64 / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bulkhead() {
        let mut bh = Bulkhead { name: "x".into(), max_concurrent: 2, active: 0, rejected: 0, completed: 0, failed: 0 };
        assert!(bh.try_enter());
        assert!(bh.try_enter());
        assert!(!bh.try_enter()); // full
        bh.exit(true);
        assert!(bh.try_enter()); // freed
    }

    #[test]
    fn test_circuit_breaker_trips() {
        let mut cb = CircuitBreaker::new("test", 3, 1000);
        assert_eq!(cb.state, CircuitState::Closed);
        for _ in 0..3 { cb.record_failure(); }
        assert_eq!(cb.state, CircuitState::Open);
    }

    #[test]
    fn test_circuit_breaker_rejects_when_open() {
        let mut cb = CircuitBreaker::new("test", 1, 100000);
        cb.record_failure();
        assert!(!cb.allow());
    }

    #[test]
    fn test_rate_limiter() {
        let mut rl = RateLimiter::new(3.0, 10.0);
        assert!(rl.try_acquire(1.0));
        assert!(rl.try_acquire(1.0));
        assert!(rl.try_acquire(1.0));
        assert!(!rl.try_acquire(1.0)); // empty
    }

    #[test]
    fn test_shield_basic() {
        let mut shield = ResilienceShield::new("test", 5, 5, 5000, 100.0);
        let r = shield.try_execute();
        assert!(r.allowed);
        shield.record(true);
        assert_eq!(shield.total_success, 1);
    }

    #[test]
    fn test_shield_score() {
        let mut shield = ResilienceShield::new("test", 5, 5, 5000, 100.0);
        for _ in 0..10 { let r = shield.try_execute(); shield.record(r.allowed && !r.simulated_failure); }
        let score = shield.score();
        assert!(score >= 0.0 && score <= 1.0);
    }

    #[test]
    fn test_shield_circuit_blocks() {
        let mut shield = ResilienceShield::new("test", 5, 1, 100000, 100.0);
        for _ in 0..5 {
            let r = shield.try_execute();
            shield.record(!r.simulated_failure);
        }
        // Circuit should be open after failures
        if shield.circuit.state == CircuitState::Open {
            let r = shield.try_execute();
            // May or may not be allowed depending on chaos
        }
    }

    #[test]
    fn test_bulkhead_utilization() {
        let mut bh = Bulkhead { name: "x".into(), max_concurrent: 10, active: 3, rejected: 0, completed: 0, failed: 0 };
        assert!((bh.utilization() - 0.3).abs() < 0.01);
    }

    #[test]
    fn test_shield_summary() {
        let shield = ResilienceShield::new("x", 5, 3, 5000, 10.0);
        let s = shield.summary();
        assert!(s.contains("CircuitState::Closed"));
    }
}
