use common::adaptation::{deploy::DeploymentInfo, Rescaler, ServerfulScalingState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Factor for moving average.
const MOVING_FACTOR: f64 = 0.25;
/// Value to force spin up.
const FORCE_THRESHOLD: f64 = 1e-4;
/// Lambda overheads
const MIN_LAMBDA_OVERHEAD: f64 = 0.007;
const MAX_LAMBDA_OVERHEAD: f64 = 0.020;
/// Hourly price of 1vcpu and 2GB.
const ECS_BASE_PRICE: f64 = 0.015;

/// Info to maintain for scaling functions.
#[derive(Serialize, Deserialize)]
struct MessagingScalingInfo {
    activity: f64,
    waiting: f64,
    // mem_usage: f64,
}

/// Rescaler for functions.
pub struct MessagingRescaler {}

impl MessagingRescaler {
    /// Initialize. Nothing to do.
    pub async fn new() -> Self {
        MessagingRescaler {}
    }
}

#[async_trait::async_trait]
impl Rescaler for MessagingRescaler {
    /// Just compute a moving average.
    async fn rescale(
        &self,
        scaling_state: &ServerfulScalingState,
        curr_timestamp: chrono::DateTime<chrono::Utc>,
        metrics: Vec<Value>,
    ) -> (u64, Value) {
        // Read deployment.
        let deployed_actor: DeploymentInfo =
            serde_json::from_value(scaling_state.deployment.clone()).unwrap();
        let deployed_actor = deployed_actor.msg_info.unwrap();
        let ecs_vcpu = deployed_actor.mem as f64 / 2048.0;
        let lambda_mem = deployed_actor.fn_mem as f64 / 1024.0;
        let caller_mem = deployed_actor.caller_mem as f64 / 1024.0;
        // Get old activity.
        let (mut activity, mut waiting) = if let Some(scaling_info) = &scaling_state.scaling_info {
            let scaling_info: MessagingScalingInfo =
                serde_json::from_value(scaling_info.clone()).unwrap();
            (scaling_info.activity, scaling_info.waiting)
        } else {
            (0.0, 0.0)
        };
        // Compute the total activity of the new metrics.
        let total_interval = curr_timestamp
            .signed_duration_since(scaling_state.last_rescale)
            .num_seconds() as f64;
        let mut total_active_secs: f64 = 0.0;
        let mut total_waiting_secs: f64 = 0.0;
        let mut force_spin_up = false;
        println!("Num metrics: {}", metrics.len());
        for m in metrics.iter() {
            let duration_secs: f64 = serde_json::from_value(m.clone()).unwrap();
            if duration_secs < FORCE_THRESHOLD {
                // Very hacky way to signal forcible spin up.
                force_spin_up = true;
            }
            // Time spent active.
            let mut active_duration_secs = duration_secs - MIN_LAMBDA_OVERHEAD;
            if active_duration_secs < 0.001 {
                // Bound by 1ms
                active_duration_secs = 0.001;
            }
            total_active_secs += active_duration_secs;
            // Average Extra time spent waiting by caller in lambda mode.
            total_waiting_secs += MAX_LAMBDA_OVERHEAD;
        }
        // Compute moving average.
        let new_activity = if !force_spin_up {
            let new_activity = total_active_secs / total_interval;
            // Limit to 1 (>1 occurs in the presence of parallism).
            if new_activity > 1.0 {
                1.0
            } else {
                new_activity
            }
        } else {
            10.0 // Forcibly spins up a new instance.
        };
        let new_waiting = total_waiting_secs / total_interval;
        activity = (1.0 - MOVING_FACTOR) * activity + MOVING_FACTOR * new_activity;
        waiting = (1.0 - MOVING_FACTOR) * waiting + MOVING_FACTOR * new_waiting;
        // Compute price ratio.
        let ecs_cost = ECS_BASE_PRICE * ecs_vcpu;
        let lambda_cost = 0.0000166667 * 3600.0 * lambda_mem * activity;
        let waiting_cost = 0.0000166667 * 3600.0 * caller_mem * waiting;
        let price_ratio = (lambda_cost + waiting_cost) / ecs_cost;
        // Set new scale.
        let new_scale = u64::from(price_ratio >= 1.0);
        let new_scaling_info = MessagingScalingInfo { activity, waiting };
        (new_scale, serde_json::to_value(&new_scaling_info).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::{MessagingRescaler, MessagingScalingInfo};
    use common::adaptation::frontend::AdapterFrontend;
    use common::adaptation::{AdapterScaling, ScalerReq};
    use common::leasing::Leaser;
    use common::{full_scaling_queue_name, scaling_table_name};
    use serde_json::Value;
    use std::sync::Arc;

    struct TestObject {
        test_start_time: chrono::DateTime<chrono::Utc>,
        scaling: AdapterScaling,
        front_end: AdapterFrontend,
        offset: u32,
    }

    impl TestObject {
        /// Create test.
        async fn new() -> TestObject {
            let subsystem = "messaging";
            let namespace = "messaging";
            let name = "echo";
            let rescaler = Arc::new(MessagingRescaler::new().await);
            let scaling = AdapterScaling::new(Some(rescaler), subsystem).await;
            let front_end = AdapterFrontend::new(subsystem, namespace, name).await;
            let t = TestObject {
                test_start_time: chrono::Utc::now(),
                scaling,
                front_end,
                offset: 0,
            };
            t.scaling.reset_revision(&t.front_end.info.namespace).await;
            // Delete lease held by previous tests.
            let scaling_table = scaling_table_name(subsystem);
            let queue_name = full_scaling_queue_name(subsystem, namespace, name);
            let leaser = Leaser::new(&scaling_table).await;
            leaser.delete(&queue_name).await;
            t
        }

        /// Populate dummy metrics for sleeper function.
        async fn populate_sleeper_metrics(&mut self, activity: f64) {
            let mut metrics: Vec<Value> = Vec::new();
            let function_duration = 1; // One function duration. Actors assume 1 second.
            let simulation_interval: u32 = 60; // Simulation duration.
            let num_entries = activity * ((simulation_interval / function_duration) as f64);
            let num_entries = num_entries.round() as i32;
            for _ in 0..num_entries {
                let duration: f64 = function_duration as f64;
                metrics.push(serde_json::to_value(duration).unwrap());
            }
            let timestamp = self
                .test_start_time
                .checked_add_signed(chrono::Duration::seconds(self.offset as i64))
                .unwrap();
            self.offset += simulation_interval;
            let req = ScalerReq {
                namespace: self.front_end.info.namespace.clone(),
                name: self.front_end.info.name.clone(),
                timestamp,
            };
            // Send in batches to test multiple reads.
            let mut lo = 0;
            while lo < metrics.len() {
                let hi = if lo + 5 < metrics.len() {
                    lo + 5
                } else {
                    metrics.len()
                };
                self.front_end.push_metrics(&metrics[lo..hi]).await;
                lo = hi;
            }
            self.scaling.handle_request(&req, false).await;
        }

        /// Check that activity is as expected.
        async fn check_activity(
            &self,
            activity_lo: f64,
            activity_hi: f64,
            expected_scaled: Option<bool>,
        ) {
            let timestamp = self
                .test_start_time
                .checked_add_signed(chrono::Duration::seconds(self.offset as i64))
                .unwrap();

            let req = ScalerReq {
                namespace: self.front_end.info.namespace.clone(),
                name: self.front_end.info.name.clone(),
                timestamp,
            };
            let scaling_state = self.scaling.handle_request(&req, false).await;
            println!("Scaling State: {scaling_state:?}");
            let scaling_info = if let Some(scaling_info) = scaling_state.scaling_info {
                serde_json::from_value(scaling_info).unwrap()
            } else {
                MessagingScalingInfo {
                    activity: 0.0,
                    waiting: 0.0,
                }
            };
            println!("Checking: lo={activity_lo}; hi={activity_hi};");
            assert!(scaling_info.activity > activity_lo);
            assert!(scaling_info.activity < activity_hi);
            if let Some(expected_scaled) = expected_scaled {
                assert!(!(expected_scaled && scaling_state.current_scale == 0));
            }
        }
    }

    /// #[tokio::test] prevents linting. So I make this a separate function.
    async fn simple_test() {
        let mut t = TestObject::new().await;
        // Initial scale should be 0.
        t.check_activity(-0.01, 0.01, Some(false)).await;
        // Curr=0.5. MA=~0.05
        t.populate_sleeper_metrics(0.5).await;
        t.check_activity(0.04, 0.06, Some(false)).await;
        // Curr=0.5. MA=~0.1
        t.populate_sleeper_metrics(0.5).await;
        t.check_activity(0.09, 0.11, Some(false)).await;
        // Curr=10. MA=~1.09.
        t.populate_sleeper_metrics(10.0).await;
        t.check_activity(1.05, 1.15, Some(true)).await;
        // Curr=10. MA=~2.
        t.populate_sleeper_metrics(10.0).await;
        t.check_activity(1.95, 2.05, Some(true)).await;
        // Curr=0.5. MA=~1.85.
        t.populate_sleeper_metrics(0.5).await;
        t.check_activity(1.8, 1.9, Some(true)).await;
    }

    #[tokio::test]
    async fn test_rescaling() {
        simple_test().await;
    }
}
