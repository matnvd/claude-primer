use crate::config::{Anchor, Config};
use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate};

/// The rolling session window: starts on your first message, expires exactly 5h later.
pub const WINDOW_HOURS: i64 = 5;

pub fn window_len() -> Duration {
    Duration::hours(WINDOW_HOURS)
}

/// Every (date, anchor) pair due in the next `days`, in chronological order, honouring
/// per-day schedules.
pub fn upcoming(cfg: &Config, days: i64) -> Result<Vec<(NaiveDate, Anchor, DateTime<Local>)>> {
    let today = cfg.today()?;
    let mode = cfg.mode()?;
    let now = Local::now();
    let mut out = Vec::new();
    for offset in 0..days {
        let date = today + Duration::days(offset);
        for a in cfg.anchors_for(date)? {
            let dt = a.local_on(date, mode)?;
            if dt > now {
                out.push((date, a, dt));
            }
        }
    }
    out.sort_by_key(|(_, _, dt)| *dt);
    Ok(out)
}

pub fn fmt_hm(d: Duration) -> String {
    let mins = d.num_minutes().max(0);
    format!("{}h{:02}m", mins / 60, mins % 60)
}

#[cfg(test)]
mod tests {
    use super::*;









    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_hm(Duration::minutes(134)), "2h14m");
        assert_eq!(fmt_hm(Duration::minutes(0)), "0h00m");
        assert_eq!(fmt_hm(Duration::minutes(-5)), "0h00m");
    }
}
