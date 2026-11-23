//! What this engine may use, as opposed to what the machine has.
//!
//! `/proc/meminfo` and `/proc/stat` describe the *host*: a container limited to
//! eight gigabytes reads 126 GiB of RAM and thirty-two cores, and a card that
//! says "7 % of 126 GiB, 32 vCPU" is describing a computer the engine cannot
//! reach. The cgroup files describe the machine the engine actually lives in, so
//! they are asked first and `/proc` is the answer only when nothing limits this
//! process.
//!
//! Every function here takes the file's text rather than reading it: the
//! arithmetic that turns "400000 100000" into four cores is the part that can be
//! wrong, and it is the part a test can pin without a container.

use std::time::Duration;

/// The used and total bytes of the memory this engine may allocate.
///
/// `memory.max` is the ceiling and `memory.current` the charge against it. A
/// `max` ceiling means nothing limits this process, and the caller falls back to
/// the machine's own numbers.
pub fn memory_used_total(memory_max: &str, memory_current: &str) -> Option<(u64, u64)> {
    let total = memory_max.trim().parse::<u64>().ok()?;
    let used = memory_current.trim().parse::<u64>().ok()?.min(total);
    (total > 0).then_some((used, total))
}

/// The cores this engine may keep busy: the quota divided by the period.
///
/// `cpu.max` is `"<quota> <period>"` in microseconds per period, or
/// `"max <period>"` when the process is not limited. One core is one period's
/// worth of CPU per period, so 400000/100000 is four cores.
pub fn cores_from_cpu_max(cpu_max: &str) -> Option<f64> {
    let mut fields = cpu_max.split_whitespace();
    let quota = fields.next()?;
    let period = fields.next()?.parse::<f64>().ok()?;
    if period <= 0.0 {
        return None;
    }
    let quota = quota.parse::<f64>().ok()?;
    let cores = quota / period;
    (cores > 0.0).then_some(cores)
}

/// The CPU percentage of the whole allowance, from the cgroup's own accounting.
///
/// `usage_usec` is CPU time, not wall-clock time: spent over `elapsed` against
/// `cores` cores, it is the fraction of the allowance that was used. Two cores
/// busy for one second on a four-core quota is 50 %, which is the number a person
/// reading the card means by "CPU".
pub fn percent_of_allowance(used_usec: u64, elapsed: Duration, cores: f64) -> Option<f64> {
    let elapsed_usec = elapsed.as_micros() as f64;
    if elapsed_usec <= 0.0 || cores <= 0.0 {
        return None;
    }
    let busy_cores = used_usec as f64 / elapsed_usec;
    Some((busy_cores / cores * 100.0).clamp(0.0, 100.0))
}

/// `usage_usec` from a `cpu.stat` file.
pub fn usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        match fields.next()? {
            "usage_usec" => fields.next()?.parse::<u64>().ok(),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_is_the_one_the_container_gets_not_the_hosts_ram() {
        // A real Pterodactyl container: 126 GiB on the host, 8 GiB allowed.
        assert_eq!(
            memory_used_total("8589934592\n", "8230629376\n"),
            Some((8230629376, 8589934592))
        );
    }

    #[test]
    fn an_unlimited_container_has_no_ceiling_to_report() {
        assert_eq!(memory_used_total("max\n", "12345\n"), None);
        assert_eq!(memory_used_total("0\n", "0\n"), None);
    }

    #[test]
    fn a_charge_above_the_ceiling_is_the_ceiling() {
        // `memory.current` can exceed `memory.max` for the moment a reclaim is
        // in flight; a card that says 110 % of a hard limit is not honest.
        assert_eq!(
            memory_used_total("100\n", "250\n"),
            Some((100, 100)),
            "used must never be reported above the ceiling"
        );
    }

    #[test]
    fn four_hundred_thousand_of_a_hundred_thousand_is_four_cores() {
        assert_eq!(cores_from_cpu_max("400000 100000\n"), Some(4.0));
        assert_eq!(cores_from_cpu_max("150000 100000\n"), Some(1.5));
    }

    #[test]
    fn an_unlimited_cpu_quota_is_no_number_of_cores() {
        assert_eq!(cores_from_cpu_max("max 100000\n"), None);
        assert_eq!(cores_from_cpu_max(""), None);
        assert_eq!(cores_from_cpu_max("400000 0\n"), None);
    }

    #[test]
    fn two_busy_cores_of_a_four_core_quota_is_half() {
        assert_eq!(
            percent_of_allowance(2_000_000, Duration::from_secs(1), 4.0),
            Some(50.0)
        );
    }

    #[test]
    fn the_allowance_is_a_ceiling_even_when_the_cgroup_spent_more() {
        // A quota that was tightened while the process was mid-sample.
        assert_eq!(
            percent_of_allowance(9_000_000, Duration::from_secs(1), 4.0),
            Some(100.0)
        );
        assert_eq!(percent_of_allowance(1_000_000, Duration::ZERO, 4.0), None);
        assert_eq!(
            percent_of_allowance(1_000_000, Duration::from_secs(1), 0.0),
            None
        );
    }

    #[test]
    fn the_usage_is_read_by_name_and_not_by_position() {
        let stat = "usage_usec 16742448682\nuser_usec 12403114150\nsystem_usec 4339334532\n";
        assert_eq!(usage_usec(stat), Some(16742448682));
        assert_eq!(usage_usec("user_usec 5\n"), None);
    }
}
