use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const DURATION_BUCKETS: [f64; 9] = [0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0];
const RATIO_BUCKETS: [f64; 7] = [0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

#[derive(Debug)]
enum Series {
    Counter {
        name: &'static str,
        help: &'static str,
        value: AtomicU64,
    },
    Gauge {
        name: &'static str,
        help: &'static str,
        value: AtomicU64,
    },
    Histogram {
        name: &'static str,
        help: &'static str,
        buckets: &'static [f64],
        counts: Vec<AtomicU64>,
        overflow: AtomicU64,
        sum_micros: AtomicU64,
        total: AtomicU64,
    },
}

impl Series {
    fn name(&self) -> &'static str {
        match self {
            Series::Counter { name, .. }
            | Series::Gauge { name, .. }
            | Series::Histogram { name, .. } => name,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Series::Counter { .. } => "counter",
            Series::Gauge { .. } => "gauge",
            Series::Histogram { .. } => "histogram",
        }
    }

    fn help(&self) -> &'static str {
        match self {
            Series::Counter { help, .. }
            | Series::Gauge { help, .. }
            | Series::Histogram { help, .. } => help,
        }
    }

    fn render(&self, out: &mut String) {
        out.push_str(&format!("# HELP {} {}\n", self.name(), self.help()));
        out.push_str(&format!("# TYPE {} {}\n", self.name(), self.kind()));
        match self {
            Series::Counter { name, value, .. } | Series::Gauge { name, value, .. } => {
                out.push_str(&format!("{name} {}\n", value.load(Ordering::Relaxed)));
            }
            Series::Histogram {
                name,
                buckets,
                counts,
                overflow,
                sum_micros,
                total,
                ..
            } => {
                let mut running = 0u64;
                for (index, bound) in buckets.iter().enumerate() {
                    running += counts[index].load(Ordering::Relaxed);
                    out.push_str(&format!(
                        "{name}_bucket{{le=\"{}\"}} {running}\n",
                        render_bound(*bound)
                    ));
                }
                let overflowed = overflow.load(Ordering::Relaxed);
                running += overflowed;
                out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {running}\n"));
                out.push_str(&format!(
                    "{name}_sum {}\n",
                    sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
                ));
                out.push_str(&format!("{name}_count {}\n", total.load(Ordering::Relaxed)));
            }
        }
    }
}

fn render_bound(bound: f64) -> String {
    if bound.fract() == 0.0 {
        format!("{}", bound as i64)
    } else {
        format!("{bound}")
    }
}

pub struct Registry {
    series: Mutex<Vec<Arc<Series>>>,
}

#[derive(Debug, Clone)]
pub struct Counter {
    series: Arc<Series>,
}

#[derive(Debug, Clone)]
pub struct Gauge {
    series: Arc<Series>,
}

#[derive(Debug, Clone)]
pub struct Histogram {
    series: Arc<Series>,
}

#[derive(Debug, Clone)]
pub struct Metrics {
    pub jobs_pending: Gauge,
    pub jobs_started: Counter,
    pub jobs_finished: Counter,
    pub claim_latency: Histogram,
    pub job_duration: Histogram,
    pub tool_run: Histogram,
    pub tool_speed: Histogram,
    pub output_bytes: Counter,
}

impl Counter {
    pub fn add(&self, amount: u64) {
        let Series::Counter { value, .. } = self.series.as_ref() else {
            return;
        };
        value.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn value(&self) -> u64 {
        match self.series.as_ref() {
            Series::Counter { value, .. } => value.load(Ordering::Relaxed),
            _ => 0,
        }
    }
}

impl Gauge {
    pub fn set(&self, amount: u64) {
        let Series::Gauge { value, .. } = self.series.as_ref() else {
            return;
        };
        value.store(amount, Ordering::Relaxed);
    }

    pub fn value(&self) -> u64 {
        match self.series.as_ref() {
            Series::Gauge { value, .. } => value.load(Ordering::Relaxed),
            _ => 0,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.value() == 0
    }
}

impl Histogram {
    pub fn observe(&self, value: f64) {
        let Series::Histogram {
            buckets,
            counts,
            overflow,
            sum_micros,
            total,
            ..
        } = self.series.as_ref()
        else {
            return;
        };
        if !value.is_finite() || value < 0.0 {
            return;
        }
        total.fetch_add(1, Ordering::Relaxed);
        sum_micros.fetch_add((value * 1_000_000.0) as u64, Ordering::Relaxed);
        match buckets.iter().position(|bound| value <= *bound) {
            Some(index) => {
                counts[index].fetch_add(1, Ordering::Relaxed);
            }
            None => {
                overflow.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn count(&self) -> u64 {
        match self.series.as_ref() {
            Series::Histogram { total, .. } => total.load(Ordering::Relaxed),
            _ => 0,
        }
    }
}

impl Registry {
    pub fn new() -> (Self, Metrics) {
        let registry = Registry {
            series: Mutex::new(Vec::new()),
        };
        let metrics = Metrics {
            jobs_pending: registry.gauge(
                "mm_jobs_pending",
                "jobs waiting to be claimed by any worker",
            ),
            jobs_started: registry.counter("mm_jobs_started_total", "jobs claimed for execution"),
            jobs_finished: registry.counter(
                "mm_jobs_finished_total",
                "jobs that reached a terminal state",
            ),
            claim_latency: registry.histogram(
                "mm_claim_latency_seconds",
                "time from a job being created to a worker claiming it",
                &DURATION_BUCKETS,
            ),
            job_duration: registry.histogram(
                "mm_job_duration_seconds",
                "time from a job being created to its terminal state",
                &DURATION_BUCKETS,
            ),
            tool_run: registry.histogram(
                "mm_tool_run_seconds",
                "wall time of one tool process",
                &DURATION_BUCKETS,
            ),
            tool_speed: registry.histogram(
                "mm_tool_speed_ratio",
                "output media seconds produced per second of tool wall time",
                &RATIO_BUCKETS,
            ),
            output_bytes: registry
                .counter("mm_output_bytes_total", "bytes written to the object store"),
        };
        (registry, metrics)
    }

    fn counter(&self, name: &'static str, help: &'static str) -> Counter {
        let series = Arc::new(Series::Counter {
            name,
            help,
            value: AtomicU64::new(0),
        });
        self.push(series.clone());
        Counter { series }
    }

    fn gauge(&self, name: &'static str, help: &'static str) -> Gauge {
        let series = Arc::new(Series::Gauge {
            name,
            help,
            value: AtomicU64::new(0),
        });
        self.push(series.clone());
        Gauge { series }
    }

    fn histogram(
        &self,
        name: &'static str,
        help: &'static str,
        buckets: &'static [f64],
    ) -> Histogram {
        let counts = (0..buckets.len()).map(|_| AtomicU64::new(0)).collect();
        let series = Arc::new(Series::Histogram {
            name,
            help,
            buckets,
            counts,
            overflow: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
            total: AtomicU64::new(0),
        });
        self.push(series.clone());
        Histogram { series }
    }

    fn push(&self, series: Arc<Series>) {
        self.series
            .lock()
            .expect("the registry lock is never poisoned")
            .push(series);
    }

    pub fn render(&self) -> String {
        let series = self
            .series
            .lock()
            .expect("the registry lock is never poisoned");
        let mut out = String::new();
        for entry in series.iter() {
            entry.render(&mut out);
        }
        out
    }
}
