const MICROS_PER_SECOND: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(0);

    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    pub const fn micros(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn seconds(self) -> f64 {
        self.0 as f64 / MICROS_PER_SECOND as f64
    }

    pub fn argument(self) -> String {
        format!(
            "{}.{:06}",
            self.0 / MICROS_PER_SECOND,
            self.0 % MICROS_PER_SECOND
        )
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.argument())
    }
}
