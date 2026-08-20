use thiserror::Error;

pub const TICKS_PER_SECOND: u64 = 254_016_000_000;
const MICROS_PER_SECOND: u128 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum TimeError {
    #[error("{field} {value} is negative or not finite")]
    NotFinite { field: &'static str, value: f64 },
    #[error("the time exceeds the largest tick count")]
    Overflow,
    #[error("rate {numerator}/{denominator} has a zero term")]
    ZeroRate { numerator: u32, denominator: u32 },
    #[error("rate {numerator}/{denominator} does not divide the tick")]
    RateNotExact { numerator: u32, denominator: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rate {
    numerator: u32,
    denominator: u32,
}

impl Rate {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, TimeError> {
        if numerator == 0 || denominator == 0 {
            return Err(TimeError::ZeroRate {
                numerator,
                denominator,
            });
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    pub fn numerator(self) -> u32 {
        self.numerator
    }

    pub fn denominator(self) -> u32 {
        self.denominator
    }

    fn ticks_per_frame(self) -> Result<u64, TimeError> {
        let scaled = u128::from(TICKS_PER_SECOND) * u128::from(self.denominator);
        let numerator = u128::from(self.numerator);
        if scaled % numerator != 0 {
            return Err(TimeError::RateNotExact {
                numerator: self.numerator,
                denominator: self.denominator,
            });
        }
        u64::try_from(scaled / numerator).map_err(|_| TimeError::Overflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ticks(u64);

impl Ticks {
    pub const ZERO: Ticks = Ticks(0);

    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    pub fn from_seconds(seconds: f64) -> Result<Self, TimeError> {
        Self::parse_seconds("seconds", seconds)
    }

    pub fn parse_seconds(field: &'static str, seconds: f64) -> Result<Self, TimeError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(TimeError::NotFinite {
                field,
                value: seconds,
            });
        }
        let scaled = (seconds * TICKS_PER_SECOND as f64).round();
        if scaled >= u64::MAX as f64 {
            return Err(TimeError::Overflow);
        }
        Ok(Self(scaled as u64))
    }

    pub fn from_frames(count: u64, rate: Rate) -> Result<Self, TimeError> {
        rate.ticks_per_frame()?
            .checked_mul(count)
            .map(Self)
            .ok_or(TimeError::Overflow)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn seconds(self) -> f64 {
        self.0 as f64 / TICKS_PER_SECOND as f64
    }

    pub fn micros(self) -> u64 {
        let ticks = u128::from(TICKS_PER_SECOND);
        let scaled = u128::from(self.0) * MICROS_PER_SECOND + ticks / 2;
        u64::try_from(scaled / ticks).unwrap_or(u64::MAX)
    }

    pub fn checked_add(self, other: Ticks) -> Option<Ticks> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_sub(self, other: Ticks) -> Option<Ticks> {
        self.0.checked_sub(other.0).map(Self)
    }
}
