use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ProgressUpdate {
    pub out_time: Duration,
    pub percent: f64,
    pub complete: bool,
}

#[derive(Debug)]
pub struct ProgressParser {
    total: Duration,
    position: Duration,
    reported: f64,
}

impl ProgressParser {
    pub fn new(total: Duration) -> Self {
        Self {
            total,
            position: Duration::ZERO,
            reported: 0.0,
        }
    }

    pub fn feed_line(&mut self, line: &str) -> Option<ProgressUpdate> {
        let (key, value) = line.trim().split_once('=')?;
        match key {
            "out_time_us" => {
                let micros: u64 = value.parse().ok()?;
                self.position = self.position.max(Duration::from_micros(micros));
                None
            }
            "out_time" => {
                let clock = parse_clock(value)?;
                self.position = self.position.max(clock);
                None
            }
            "progress" => {
                let complete = value == "end";
                let percent = if complete {
                    100.0
                } else {
                    self.running_percent()
                };
                self.reported = self.reported.max(percent);
                Some(ProgressUpdate {
                    out_time: self.position,
                    percent: self.reported,
                    complete,
                })
            }
            _ => None,
        }
    }

    fn running_percent(&self) -> f64 {
        if self.total.is_zero() {
            return 0.0;
        }
        (self.position.as_secs_f64() / self.total.as_secs_f64() * 100.0).clamp(0.0, 100.0)
    }
}

fn parse_clock(value: &str) -> Option<Duration> {
    let (clock, fraction) = value.split_once('.').unwrap_or((value, "0"));
    let mut parts = clock.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let padded = format!("{fraction:0<6}");
    let micros: u64 = padded.get(..6)?.parse().ok()?;
    Some(Duration::from_secs(hours * 3600 + minutes * 60 + seconds) + Duration::from_micros(micros))
}
