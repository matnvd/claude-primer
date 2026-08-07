use crate::config::{Anchor, Config};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime};

/// The rolling session window: starts on your first message, expires exactly 5h later.
pub const WINDOW_HOURS: i64 = 5;

pub fn window_len() -> Duration {
    Duration::hours(WINDOW_HOURS)
}

/// One simulated window opened by an anchor.
#[derive(Debug, Clone)]
pub struct SimWindow {
    pub opened_by: Anchor,
    pub start: NaiveTime,
    pub end: NaiveTime,
    /// Minutes of the workday this window covers.
    pub covers_minutes: i64,
    pub covers_from: Option<NaiveTime>,
    pub covers_to: Option<NaiveTime>,
}

/// An anchor that fired while a window was already open: it spends quota but opens
/// nothing, and every later boundary shifts as a result.
#[derive(Debug, Clone)]
pub struct WastedAnchor {
    pub anchor: Anchor,
    pub window_open_until: NaiveTime,
}

#[derive(Debug)]
pub struct Simulation {
    pub windows: Vec<SimWindow>,
    pub wasted: Vec<WastedAnchor>,
    pub workday_minutes: i64,
    pub covered_minutes: i64,
}

impl Simulation {
    pub fn windows_touching_workday(&self) -> usize {
        self.windows.iter().filter(|w| w.covers_minutes > 0).count()
    }

    pub fn coverage_pct(&self) -> f64 {
        if self.workday_minutes == 0 {
            return 0.0;
        }
        100.0 * self.covered_minutes as f64 / self.workday_minutes as f64
    }
}

/// Walk the anchors in order, tracking whether a window is already open. An anchor
/// only opens a new window if the previous one has expired; otherwise it is wasted.
pub fn simulate(anchors: &[Anchor], workday: (NaiveTime, NaiveTime)) -> Simulation {
    let (day_start, day_end) = workday;
    let mut windows: Vec<SimWindow> = Vec::new();
    let mut wasted: Vec<WastedAnchor> = Vec::new();
    let mut open_until: Option<NaiveTime> = None;

    let mut sorted = anchors.to_vec();
    sorted.sort();

    for a in sorted {
        let start = NaiveTime::from_hms_opt(a.hour, a.minute, 0).expect("validated anchor");
        if let Some(until) = open_until {
            if start < until {
                wasted.push(WastedAnchor { anchor: a, window_open_until: until });
                continue;
            }
        }
        let end = start + window_len();
        open_until = Some(end);

        let ov_start = start.max(day_start);
        let ov_end = end.min(day_end);
        let covers = if ov_end > ov_start {
            (ov_end - ov_start).num_minutes()
        } else {
            0
        };

        windows.push(SimWindow {
            opened_by: a,
            start,
            end,
            covers_minutes: covers,
            covers_from: (covers > 0).then_some(ov_start),
            covers_to: (covers > 0).then_some(ov_end),
        });
    }

    let workday_minutes = (day_end - day_start).num_minutes().max(0);
    let covered_minutes: i64 = windows.iter().map(|w| w.covers_minutes).sum::<i64>().min(workday_minutes);

    Simulation { windows, wasted, workday_minutes, covered_minutes }
}

pub fn parse_workday(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("workday must look like 09:00-17:00"))?;
    let start = NaiveTime::parse_from_str(a.trim(), "%H:%M").context("bad workday start")?;
    let end = NaiveTime::parse_from_str(b.trim(), "%H:%M").context("bad workday end")?;
    if end <= start {
        return Err(anyhow!("workday end must be after start"));
    }
    Ok((start, end))
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

    fn a(s: &str) -> Anchor {
        Anchor::parse(s).unwrap()
    }

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    #[test]
    fn well_spaced_anchors_all_open_windows() {
        let sim = simulate(&[a("05:30"), a("10:30"), a("15:30")], (t("09:00"), t("17:00")));
        assert_eq!(sim.windows.len(), 3);
        assert!(sim.wasted.is_empty());
        assert_eq!(sim.windows[0].end, t("10:30"));
        assert_eq!(sim.windows[2].end, t("20:30"));
    }

    #[test]
    fn an_anchor_inside_an_open_window_is_wasted() {
        // 08:00 lands inside the window 05:30 opened, so it spends quota for nothing.
        let sim = simulate(&[a("05:30"), a("08:00"), a("10:30")], (t("09:00"), t("17:00")));
        assert_eq!(sim.wasted.len(), 1);
        assert_eq!(sim.wasted[0].anchor, a("08:00"));
        assert_eq!(sim.wasted[0].window_open_until, t("10:30"));
        assert_eq!(sim.windows.len(), 2);
    }

    #[test]
    fn early_anchoring_touches_three_windows_and_covers_the_day() {
        let sim = simulate(&[a("05:30"), a("10:30"), a("15:30")], (t("09:00"), t("17:00")));
        assert_eq!(sim.windows_touching_workday(), 3);
        assert_eq!(sim.covered_minutes, 480);
        assert!((sim.coverage_pct() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn anchoring_at_start_of_work_touches_only_two() {
        let sim = simulate(&[a("09:00"), a("14:00")], (t("09:00"), t("17:00")));
        assert_eq!(sim.windows_touching_workday(), 2);
        assert_eq!(sim.covered_minutes, 480);
    }

    #[test]
    fn coverage_can_be_partial() {
        // One window only: 09:00-14:00 against a 09:00-17:00 day leaves 3h uncovered.
        let sim = simulate(&[a("09:00")], (t("09:00"), t("17:00")));
        assert_eq!(sim.covered_minutes, 300);
        assert!((sim.coverage_pct() - 62.5).abs() < 0.01);
    }

    #[test]
    fn workday_parsing_rejects_inverted_ranges() {
        assert!(parse_workday("09:00-17:00").is_ok());
        assert!(parse_workday("17:00-09:00").is_err());
        assert!(parse_workday("09:00").is_err());
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_hm(Duration::minutes(134)), "2h14m");
        assert_eq!(fmt_hm(Duration::minutes(0)), "0h00m");
        assert_eq!(fmt_hm(Duration::minutes(-5)), "0h00m");
    }
}
