//! Auto-backup: triggers a backup when CPU has been idle for a sustained period.
//!
//! Spawns a tokio background task that polls `/proc/stat` every 15 seconds,
//! tracks a continuous idle streak, and when the streak exceeds the configured
//! duration claims the backup slot and starts a backup on an OS thread (same
//! path as a manual trigger). Disabled until explicitly turned on via config.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::backup;
use crate::gateway::Gateway;

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_IDLE_SECS: u64 = 300;
const DEFAULT_CPU_THRESHOLD: f64 = 10.0;
const DEFAULT_COOLDOWN_SECS: u64 = 1800;

/// Live state visible to the API layer for status reporting.
static AUTO_STATE: OnceLock<AutoState> = OnceLock::new();
static CPU_HISTORY: OnceLock<Mutex<VecDeque<f64>>> = OnceLock::new();

const CPU_HISTORY_LIMIT: usize = 60;

fn cpu_history() -> &'static Mutex<VecDeque<f64>> {
    CPU_HISTORY.get_or_init(|| Mutex::new(VecDeque::with_capacity(CPU_HISTORY_LIMIT)))
}

fn record_cpu_sample(value: f64) {
    let mut history = cpu_history().lock().unwrap_or_else(|e| e.into_inner());
    if history.len() == CPU_HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(value.clamp(0.0, 100.0));
}

/// Auto-backup stays locked until this process has completed a successful restore.
pub fn mark_restore_complete() {
    state().restore_ready.store(true, Ordering::Release);
}

pub struct AutoState {
    pub idle_streak_secs: AtomicU64,
    pub cpu_current_pct: AtomicU64,
    pub last_auto_backup_at: AtomicI64,
    pub enabled: AtomicBool,
    pub restore_ready: AtomicBool,
}

fn state() -> &'static AutoState {
    AUTO_STATE.get_or_init(|| AutoState {
        idle_streak_secs: AtomicU64::new(0),
        cpu_current_pct: AtomicU64::new(0),
        last_auto_backup_at: AtomicI64::new(0),
        enabled: AtomicBool::new(false),
        restore_ready: AtomicBool::new(false),
    })
}

pub fn auto_status_json() -> serde_json::Value {
    let s = state();
    serde_json::json!({
        "enabled": s.enabled.load(Ordering::Relaxed),
        "idle_streak_secs": s.idle_streak_secs.load(Ordering::Relaxed),
        "cpu_current": s.cpu_current_pct.load(Ordering::Relaxed) as f64 / 10.0,
        "last_auto_backup_at": s.last_auto_backup_at.load(Ordering::Relaxed),
        "restore_ready": s.restore_ready.load(Ordering::Acquire),
        "cpu_history": cpu_history().lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect::<Vec<_>>(),
    })
}

/// The auto-backup configuration, merged from the stored backup config.
#[derive(Clone, Debug)]
struct AutoConfig {
    enabled: bool,
    idle_secs: u64,
    cpu_threshold: f64,
    cooldown_secs: u64,
}

/// Spawn the auto-backup polling loop. Call once after the gateway is up.
pub fn spawn(g: Arc<Gateway>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut prev_cpu: Option<CpuTimes> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            let cfg = match read_auto_config(&g).await {
                Some(cfg) if cfg.enabled => cfg,
                _ => {
                    state().enabled.store(false, Ordering::Relaxed);
                    state().idle_streak_secs.store(0, Ordering::Relaxed);
                    prev_cpu = None;
                    continue;
                }
            };
            state().enabled.store(true, Ordering::Relaxed);

            let current = match read_proc_stat() {
                Some(times) => times,
                None => continue,
            };

            let cpu_pct = match prev_cpu {
                Some(prev) => compute_cpu_pct(&prev, &current),
                None => {
                    prev_cpu = Some(current);
                    continue;
                }
            };
            prev_cpu = Some(current);

            state()
                .cpu_current_pct
                .store((cpu_pct * 10.0) as u64, Ordering::Relaxed);
            record_cpu_sample(cpu_pct);

            if !state().restore_ready.load(Ordering::Acquire) {
                state().idle_streak_secs.store(0, Ordering::Relaxed);
                continue;
            }

            if cpu_pct < cfg.cpu_threshold {
                let streak = state()
                    .idle_streak_secs
                    .fetch_add(POLL_INTERVAL.as_secs(), Ordering::Relaxed)
                    + POLL_INTERVAL.as_secs();

                if streak >= cfg.idle_secs {
                    let last_run = state().last_auto_backup_at.load(Ordering::Relaxed);
                    let now = aixodia_backup::now_unix();
                    if now - last_run < cfg.cooldown_secs as i64 {
                        continue;
                    }
                    tracing::info!(
                        "auto-backup: idle {streak}s (threshold {}s, cpu {cpu_pct:.1}%), triggering",
                        cfg.idle_secs
                    );
                    if try_start_auto_backup(&g).await {
                        state()
                            .last_auto_backup_at
                            .store(aixodia_backup::now_unix(), Ordering::Relaxed);
                        state().idle_streak_secs.store(0, Ordering::Relaxed);
                    }
                }
            } else {
                state().idle_streak_secs.store(0, Ordering::Relaxed);
            }
        }
    });
}

/// Read backup config from D1 and extract the auto-backup fields.
async fn read_auto_config(g: &Gateway) -> Option<AutoConfig> {
    let store = g.state_store()?;
    let raw = store.get_state(backup::BACKUP_CONFIG_KEY).await.ok()??;
    let parsed: backup::Stored = serde_json::from_str(&raw).ok()?;
    Some(AutoConfig {
        enabled: parsed.auto_enabled.unwrap_or(false),
        idle_secs: parsed.auto_idle_secs.unwrap_or(DEFAULT_IDLE_SECS),
        cpu_threshold: parsed.auto_cpu_threshold.unwrap_or(DEFAULT_CPU_THRESHOLD),
        cooldown_secs: parsed.auto_cooldown_secs.unwrap_or(DEFAULT_COOLDOWN_SECS),
    })
}

/// Attempt to start a backup through the existing slot mechanism.
async fn try_start_auto_backup(g: &Gateway) -> bool {
    let store = match g.state_store() {
        Some(s) => s,
        None => return false,
    };
    let raw = match store.get_state(backup::BACKUP_CONFIG_KEY).await {
        Ok(Some(raw)) => raw,
        _ => return false,
    };
    let config = backup::resolve(Some(&raw));
    if !backup::ready(&config, backup::BACKUP_RUN) {
        return false;
    }
    backup::claim_and_spawn_backup(config, store)
}

// --- /proc/stat CPU usage measurement ---

struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

fn read_proc_stat() -> Option<CpuTimes> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            let fields: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if fields.len() >= 8 {
                return Some(CpuTimes {
                    user: fields[0],
                    nice: fields[1],
                    system: fields[2],
                    idle: fields[3],
                    iowait: fields[4],
                    irq: fields[5],
                    softirq: fields[6],
                });
            }
            if fields.len() >= 5 {
                return Some(CpuTimes {
                    user: fields[0],
                    nice: fields[1],
                    system: fields[2],
                    idle: fields[3],
                    iowait: fields[4],
                    irq: 0,
                    softirq: 0,
                });
            }
            return None;
        }
    }
    None
}

/// Compute overall CPU usage percentage between two snapshots.
fn compute_cpu_pct(prev: &CpuTimes, cur: &CpuTimes) -> f64 {
    let prev_total =
        prev.user + prev.nice + prev.system + prev.idle + prev.iowait + prev.irq + prev.softirq;
    let cur_total =
        cur.user + cur.nice + cur.system + cur.idle + cur.iowait + cur.irq + cur.softirq;
    let delta_total = cur_total.saturating_sub(prev_total) as f64;
    if delta_total == 0.0 {
        return 0.0;
    }
    let delta_idle = (cur.idle + cur.iowait).saturating_sub(prev.idle + prev.iowait) as f64;
    ((delta_total - delta_idle) / delta_total) * 100.0
}
