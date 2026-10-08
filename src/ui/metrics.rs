//! How each metric is titled, formatted and coloured. A backend only names what it reads ("hit");
//! this is where the screen decides what that means.

use ratatui::style::Color;

use super::text::{fmt_bytes, fmt_count, fmt_secs};
use super::theme::{Gradient, THEME};

#[derive(Clone, Copy)]
pub enum Unit {
    PerSec,
    BytesPerSec,
    Percent,
    Count,
    Seconds,
}

/// When a value needs a look.
#[derive(Clone, Copy)]
pub enum Alert {
    Never,
    Above { warn: f64, crit: f64 },
    Below { warn: f64, crit: f64 },
}

pub struct Spec {
    /// Short: it shares a panel's top edge with the value, and some panels are 32 columns wide.
    pub title: &'static str,
    pub unit: Unit,
    /// Heat colours where more is worse; cool ones for plain activity.
    pub heat: bool,
    /// The chart's top never goes below this, so idle noise doesn't fill the panel.
    pub floor: f64,
    /// A chart that always spans the same range, like a percentage.
    pub fixed_max: Option<f64>,
    pub alert: Alert,
}

impl Spec {
    pub fn gradient(&self) -> Gradient {
        if self.heat { THEME.heat } else { THEME.cool }
    }

    pub fn format(&self, value: f64) -> String {
        match self.unit {
            Unit::PerSec => format!("{}/s", fmt_count(value)),
            Unit::BytesPerSec => format!("{}/s", fmt_bytes(value)),
            Unit::Percent => format!("{value:.1}%"),
            Unit::Count => fmt_count(value),
            Unit::Seconds => fmt_secs(value),
        }
    }

    /// The colour of the value in the panel's title.
    pub fn tone(&self, value: f64) -> Color {
        match self.alert {
            Alert::Never => THEME.text,
            Alert::Above { crit, .. } if value >= crit => THEME.crit,
            Alert::Above { warn, .. } if value >= warn => THEME.warn,
            Alert::Below { crit, .. } if value <= crit => THEME.crit,
            Alert::Below { warn, .. } if value <= warn => THEME.warn,
            _ => THEME.text,
        }
    }
}

/// Every metric the backends report. `None` for a key nobody here knows, which is not drawn.
pub fn spec(key: &str) -> Option<&'static Spec> {
    use Alert::{Above, Below, Never};
    use Unit::{BytesPerSec, Count, PerSec, Percent, Seconds};
    Some(match key {
        "qps" => &Spec {
            title: "QPS",
            unit: PerSec,
            heat: false,
            floor: 10.0,
            fixed_max: None,
            alert: Never,
        },
        "tps" => &Spec {
            title: "TPS",
            unit: PerSec,
            heat: false,
            floor: 5.0,
            fixed_max: None,
            alert: Never,
        },
        "slow" => &Spec {
            title: "Slow",
            unit: PerSec,
            heat: true,
            floor: 1.0,
            fixed_max: None,
            alert: Above {
                warn: 0.5,
                crit: 5.0,
            },
        },
        "hit" => &Spec {
            title: "Buffer Pool Hit",
            unit: Percent,
            heat: false,
            floor: 100.0,
            fixed_max: Some(100.0),
            alert: Below {
                warn: 95.0,
                crit: 85.0,
            },
        },
        "lock_waits" => &Spec {
            title: "Lock Waits",
            unit: Count,
            heat: true,
            floor: 2.0,
            fixed_max: None,
            alert: Above {
                warn: 1.0,
                crit: 5.0,
            },
        },
        "deadlocks" => &Spec {
            title: "Deadlocks",
            unit: PerSec,
            heat: true,
            floor: 1.0,
            fixed_max: None,
            alert: Above {
                warn: 0.5,
                crit: 5.0,
            },
        },
        "oldest_xact" => &Spec {
            title: "Oldest Xact",
            unit: Seconds,
            heat: true,
            floor: 10.0,
            fixed_max: None,
            alert: Above {
                warn: 60.0,
                crit: 600.0,
            },
        },
        // History list length: undo records no one has purged yet, which a long transaction (or
        // a purge that cannot keep up) lets pile up. Thousands are ordinary on a busy server.
        "hll" => &Spec {
            title: "History List",
            unit: Count,
            heat: true,
            floor: 100.0,
            fixed_max: None,
            alert: Above {
                warn: 100_000.0,
                crit: 1_000_000.0,
            },
        },
        "idle_xact" => &Spec {
            title: "Idle in Xact",
            unit: Count,
            heat: true,
            floor: 2.0,
            fixed_max: None,
            alert: Above {
                warn: 1.0,
                crit: 5.0,
            },
        },
        "net" => &Spec {
            title: "Net",
            unit: BytesPerSec,
            heat: false,
            floor: 1024.0,
            fixed_max: None,
            alert: Never,
        },
        "tmp_disk" => &Spec {
            title: "Tmp Disk",
            unit: PerSec,
            heat: true,
            floor: 1.0,
            fixed_max: None,
            alert: Above {
                warn: 1.0,
                crit: 10.0,
            },
        },
        "temp_bytes" => &Spec {
            title: "Temp Files",
            unit: BytesPerSec,
            heat: true,
            floor: 1024.0,
            fixed_max: None,
            alert: Above {
                warn: 1e6,
                crit: 1e8,
            },
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 12] = [
        "qps",
        "tps",
        "slow",
        "hit",
        "lock_waits",
        "deadlocks",
        "oldest_xact",
        "hll",
        "idle_xact",
        "net",
        "tmp_disk",
        "temp_bytes",
    ];

    #[test]
    fn every_key_has_a_short_title_and_formats_any_value() {
        for key in KEYS {
            let spec = spec(key).unwrap_or_else(|| panic!("{key} has no spec"));
            assert!(
                spec.title.chars().count() <= 16,
                "{key}: {:?} is too long for a narrow panel",
                spec.title
            );
            for value in [0.0, 0.4, 7.0, 1234.5, 9.9e9] {
                assert!(!spec.format(value).is_empty(), "{key} {value}");
            }
        }
        assert!(spec("nonsense").is_none());
    }

    #[test]
    fn values_are_formatted_in_their_own_unit() {
        let get = |key: &str, value: f64| spec(key).unwrap().format(value);
        assert_eq!(get("qps", 1234.0), "1.2k/s");
        assert_eq!(get("slow", 0.3), "0.3/s");
        assert_eq!(get("hit", 99.123), "99.1%");
        assert_eq!(get("net", 1536.0), "1.5 KB/s");
        assert_eq!(get("oldest_xact", 125.0), "2m05s");
        assert_eq!(get("lock_waits", 3.0), "3");
        assert_eq!(get("hll", 4889.0), "4.9k");
        assert_eq!(get("deadlocks", 1.0), "1/s");
    }

    #[test]
    fn a_value_that_needs_a_look_changes_colour() {
        let hit = spec("hit").unwrap();
        assert_eq!(hit.tone(99.0), THEME.text);
        assert_eq!(
            hit.tone(90.0),
            THEME.warn,
            "low is the bad direction for a hit rate"
        );
        assert_eq!(hit.tone(80.0), THEME.crit);

        let waits = spec("lock_waits").unwrap();
        assert_eq!(waits.tone(0.0), THEME.text);
        assert_eq!(
            waits.tone(1.0),
            THEME.warn,
            "a single wait is already worth seeing"
        );
        assert_eq!(waits.tone(5.0), THEME.crit);

        let deadlocks = spec("deadlocks").unwrap();
        assert_eq!(deadlocks.tone(0.0), THEME.text);
        assert_eq!(deadlocks.tone(1.0), THEME.warn, "a deadlock is always news");

        let history = spec("hll").unwrap();
        assert_eq!(history.tone(10_136.0), THEME.text, "thousands are ordinary");
        assert_eq!(history.tone(150_000.0), THEME.warn);
        assert_eq!(history.tone(2e6), THEME.crit);

        assert_eq!(
            spec("qps").unwrap().tone(1e9),
            THEME.text,
            "plain activity never alarms"
        );
    }

    #[test]
    fn only_the_percentage_has_a_fixed_range() {
        for key in KEYS {
            assert_eq!(
                spec(key).unwrap().fixed_max.is_some(),
                key == "hit",
                "{key}"
            );
        }
    }
}
