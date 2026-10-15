//! Admission at the actual model/tool dispatch boundary. All child runs may
//! share one Scheduler; cooperating CLI processes also share file-lock quotas.
//! Budgets count conservative reservations until the provider reports usage.
use crate::{Config, coordinator, shared_limits};
use mio_agent::{
    agent::{
        AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
        StepEventKind,
    },
    run::PromptError,
};
use mio_core::{
    effect::{EffectId, EffectKind, Outcome},
    error::{ErrorKind, ErrorReport},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Shared policy for one host process. Cross-process limits, when enabled,
/// apply only to cooperating processes using the same directory and policy.
#[derive(Clone)]
pub struct Scheduler {
    pub coordinator: coordinator::Coordinator,
    pub shared: Option<shared_limits::SharedLimits>,
    waiting_shared_model: Arc<AtomicUsize>,
    waiting_shared_heavy: Arc<AtomicUsize>,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("limits", self.coordinator.limits())
            .field("cross_process", &self.shared.is_some())
            .finish()
    }
}

impl Scheduler {
    pub fn new(
        limits: coordinator::Limits,
        shared_directory: Option<&Path>,
    ) -> Result<Self, String> {
        let coordinator =
            coordinator::Coordinator::new(limits).map_err(|error| error.to_string())?;
        // The cross-process policy has its own stable defaults. Local flags
        // narrow admission independently; changing one process never expands
        // the shared policy or silently rewrites it for other processes.
        let shared = shared_directory
            .map(|directory| {
                shared_limits::SharedLimits::new(
                    directory,
                    shared_limits::SharedLimitsConfig::default(),
                )
            })
            .transpose()
            .map_err(|error| error.to_string())?;
        Ok(Self::from_parts(coordinator, shared))
    }

    /// Embed a custom shared policy without replacing another process's policy.
    pub fn from_parts(
        coordinator: coordinator::Coordinator,
        shared: Option<shared_limits::SharedLimits>,
    ) -> Self {
        Self {
            coordinator,
            shared,
            waiting_shared_model: Arc::new(AtomicUsize::new(0)),
            waiting_shared_heavy: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn snapshot(&self) -> Value {
        let local = self.coordinator.snapshot();
        let shared = self.shared.as_ref().map(|shared| match shared.snapshot() {
            Ok(snapshot) => json!({
                "modelLimit": snapshot.config.global_model_concurrency,
                "providerLimit": snapshot.config.provider_model_concurrency,
                "heavyLimit": snapshot.config.heavy_concurrency,
                "cooldowns": snapshot.cooldowns.iter().map(|cooldown| json!({
                    "providerHash": cooldown.provider_hash,
                    "remainingMs": cooldown.remaining.as_millis(),
                })).collect::<Vec<_>>(),
            }),
            Err(_) => {
                json!({"error": "Shared quota state is unavailable; admission will fail closed"})
            }
        });
        json!({
            "tasks": {"active": local.tasks.active, "queued": local.tasks.queued, "limit": local.tasks.limit},
            "model": {"active": local.model.active_total, "queued": local.model.queued, "limit": local.model.limit,
                "activeByProviderHash": local.model.active_by_provider,
                "cooldownMs": local.model.cooling.iter().map(|(key, delay)| (key, delay.as_millis())).collect::<HashMap<_,_>>()},
            "heavy": {"active": local.heavy.active, "queued": local.heavy.queued, "limit": local.heavy.limit},
            "budget": {"calls": local.budget.model_calls_started, "maxCalls": local.budget.max_model_calls,
                "tokensReserved": local.budget.tokens_reserved_active, "tokensUsed": local.budget.tokens_used,
                "maxTokens": local.budget.max_total_tokens, "overage": local.budget.tokens_overage,
                "blocked": local.budget.blocked},
            "shared": shared,
            "waitingSharedModel": self.waiting_shared_model.load(Ordering::SeqCst),
            "waitingSharedHeavy": self.waiting_shared_heavy.load(Ordering::SeqCst),
        })
    }
}

struct ModelLease {
    local: coordinator::ModelPermit,
    _shared: Option<shared_limits::ModelPermit>,
}
struct Waiting<'a>(&'a AtomicUsize);
impl<'a> Waiting<'a> {
    fn new(count: &'a AtomicUsize, capacity: usize) -> Result<Self, String> {
        count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                (value < capacity).then_some(value + 1)
            })
            .map_err(|_| "Shared admission queue is full".to_owned())?;
        Ok(Self(count))
    }
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct HeavyLease {
    _local: coordinator::HeavyPermit,
    _shared: Option<shared_limits::HeavyPermit>,
}
struct RunAdmission {
    scheduler: Arc<Scheduler>,
    provider_key: String,
    models: Mutex<HashMap<EffectId, ModelLease>>,
    heavy: Mutex<HashMap<EffectId, HeavyLease>>,
}

/// Each hook instance belongs to one run. Clones share its live permits so
/// both normal outcomes and the stream's terminal error can release them.
#[derive(Clone)]
pub(crate) struct SchedulingHook(Arc<RunAdmission>);

impl SchedulingHook {
    pub(crate) fn new(scheduler: Arc<Scheduler>, config: &Config) -> Self {
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&config.provider).expect("provider serializes"));
        hash.update([0]);
        hash.update(config.base_url.trim_end_matches('/').as_bytes());
        hash.update([0]);
        hash.update(config.api_key.as_bytes());
        Self(Arc::new(RunAdmission {
            scheduler,
            provider_key: format!("{:x}", hash.finalize()),
            models: Mutex::new(HashMap::new()),
            heavy: Mutex::new(HashMap::new()),
        }))
    }

    async fn reserve_model(
        &self,
        id: EffectId,
        request: &mio_core::completion::CompletionRequest,
    ) -> Result<(), String> {
        let size = serde_json::to_vec(request)
            .map_err(|_| "Cannot account for model request")?
            .len();
        // Serialized UTF-8 bytes deliberately overestimate ordinary text token
        // counts. This is a conservative admission estimate, not a tokenizer.
        let reservation = (size as u64).saturating_add(request.max_tokens.unwrap_or(4096));
        let shared = match &self.0.scheduler.shared {
            Some(shared) => {
                let _waiting = Waiting::new(
                    &self.0.scheduler.waiting_shared_model,
                    self.0.scheduler.coordinator.limits().queue_capacity,
                )?;
                Some(
                    shared
                        .acquire_model(&self.0.provider_key)
                        .await
                        .map_err(|_| "Shared model admission failed; inspect quota state")?,
                )
            }
            None => None,
        };
        let local = self
            .0
            .scheduler
            .coordinator
            .acquire_model(&self.0.provider_key, reservation)
            .await
            .map_err(|error| error.to_string())?;
        self.0.models.lock().unwrap().insert(
            id,
            ModelLease {
                local,
                _shared: shared,
            },
        );
        Ok(())
    }

    async fn reserve_heavy(&self, id: EffectId) -> Result<(), String> {
        let shared = match &self.0.scheduler.shared {
            Some(shared) => {
                let _waiting = Waiting::new(
                    &self.0.scheduler.waiting_shared_heavy,
                    self.0.scheduler.coordinator.limits().queue_capacity,
                )?;
                Some(
                    shared
                        .acquire_heavy()
                        .await
                        .map_err(|_| "Shared heavy-task admission failed; inspect quota state")?,
                )
            }
            None => None,
        };
        let local = self
            .0
            .scheduler
            .coordinator
            .acquire_heavy()
            .await
            .map_err(|error| error.to_string())?;
        self.0.heavy.lock().unwrap().insert(
            id,
            HeavyLease {
                _local: local,
                _shared: shared,
            },
        );
        Ok(())
    }

    /// A failed stream has no successful completion outcome. Share throttling
    /// before releasing slots, and charge unknown usage conservatively. Never
    /// replay a turn: earlier tools or streamed output may already have effects.
    pub(crate) async fn failed(&self, error: &PromptError) -> Result<(), String> {
        if error
            .provider_response_status()
            .is_some_and(|status| status.as_u16() == 429)
        {
            let header = error
                .provider_response_headers()
                .and_then(|headers| headers.get("retry-after"))
                .and_then(|value| value.to_str().ok());
            let delay = retry_after(header, SystemTime::now());
            self.0
                .scheduler
                .coordinator
                .defer_provider(&self.0.provider_key, delay);
            if let Some(shared) = &self.0.scheduler.shared {
                shared
                    .defer_provider(&self.0.provider_key, delay)
                    .await
                    .map_err(|_| {
                        "Scheduling: could not publish shared provider cooldown".to_owned()
                    })?;
            }
        }
        self.0.models.lock().unwrap().clear();
        self.0.heavy.lock().unwrap().clear();
        Ok(())
    }
}

fn heavy_tool(name: &str, args: &str) -> bool {
    match name {
        "bash" | "shell" => true,
        "workspace" => serde_json::from_str::<Value>(args)
            .ok()
            .and_then(|value| {
                value
                    .get("operation")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|operation| matches!(operation.as_str(), "bash" | "shell")),
        name => name.starts_with("plugin__") || name.starts_with("mcp__"),
    }
}

impl AgentHook for SchedulingHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionDispatch | StepEventKind::ToolDispatch
        )
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let admission = match event.kind {
            EffectKind::Completion { request, .. } => self.reserve_model(event.id, request).await,
            EffectKind::ToolCall { name, args } if heavy_tool(name, args) => {
                self.reserve_heavy(event.id).await
            }
            _ => Ok(()),
        };
        match admission {
            Ok(()) => DispatchAction::Proceed,
            Err(error) => DispatchAction::deny(ErrorReport::new(
                ErrorKind::Cancelled,
                format!("Scheduling: {error}"),
            )),
        }
    }

    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(lease) = self.0.models.lock().unwrap().remove(&event.id) {
            let usage = match event.outcome {
                Ok(Outcome::Completion(response)) => response.usage.total_tokens.or_else(|| {
                    response
                        .usage
                        .input_tokens
                        .zip(response.usage.output_tokens)
                        .map(|(input, output)| input.saturating_add(output))
                }),
                _ => None,
            };
            lease.local.complete(usage);
        }
        self.0.heavy.lock().unwrap().remove(&event.id);
        OutcomeAction::Proceed
    }
}

// Retry-After supports delta-seconds and modern IMF-fixdate. Invalid headers
// use a five-second shared cooldown; there is no automatic replay or retry.
fn retry_after(header: Option<&str>, now: SystemTime) -> Duration {
    let value = header.unwrap_or("").trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Duration::from_secs(seconds).max(Duration::from_millis(100));
    }
    if let Some(deadline) = http_date(value) {
        return deadline
            .duration_since(now)
            .unwrap_or_default()
            .max(Duration::from_millis(100));
    }
    Duration::from_secs(5)
}

fn http_date(value: &str) -> Option<SystemTime> {
    let parts = value.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || !parts[0].ends_with(',') || parts[5] != "GMT" {
        return None;
    }
    let day = parts[1].parse::<u64>().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == parts[2])?;
    let year = parts[3].parse::<u64>().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > months[month] {
        return None;
    }
    let clock = parts[4]
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if clock.len() != 3 || clock[0] > 23 || clock[1] > 59 || clock[2] > 59 {
        return None;
    }
    let leaps_before = |year: u64| {
        let prior = year - 1;
        prior / 4 - prior / 100 + prior / 400
    };
    let days = (year - 1970) * 365 + leaps_before(year) - leaps_before(1970)
        + months[..month].iter().sum::<u64>()
        + day
        - 1;
    UNIX_EPOCH.checked_add(Duration::from_secs(
        days * 86400 + clock[0] * 3600 + clock[1] * 60 + clock[2],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_seconds_dates_and_invalid_headers() {
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(UNIX_EPOCH));
        assert_eq!(
            http_date("Sat, 01 Jan 2000 00:00:00 GMT"),
            Some(UNIX_EPOCH + Duration::from_secs(946684800))
        );
        assert!(http_date("Sun, 31 Feb 2026 00:00:00 GMT").is_none());
        assert_eq!(retry_after(Some("3"), UNIX_EPOCH), Duration::from_secs(3));
        assert_eq!(
            retry_after(Some("Thu, 01 Jan 1970 00:00:02 GMT"), UNIX_EPOCH),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_after(Some("nonsense"), UNIX_EPOCH),
            Duration::from_secs(5)
        );
    }
    #[test]
    fn heavy_classification_includes_aliases_and_extensions() {
        assert!(heavy_tool("workspace", r#"{"operation":"shell"}"#));
        assert!(heavy_tool("bash", "{}"));
        assert!(heavy_tool("mcp__server__tool", "{}"));
        assert!(heavy_tool("plugin__a__b", "{}"));
        assert!(!heavy_tool("read", "{}"));
    }
}
