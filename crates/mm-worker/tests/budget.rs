use std::path::Path;

use mm_worker::{DiskBudget, admits};

#[test]
fn admission_accounts_for_reservations() {
    assert!(admits(100, 1000, 0));
    assert!(admits(100, 1000, 900));
    assert!(!admits(101, 1000, 900));
    assert!(!admits(2000, 1000, 0));
}

#[test]
fn a_fixed_budget_reports_its_size() {
    assert_eq!(
        DiskBudget::Fixed(4096).available(Path::new(".")),
        Some(4096)
    );
}

#[test]
fn probing_the_working_directory_reports_space() {
    let available = DiskBudget::Probe
        .available(Path::new("."))
        .expect("the working directory is statable");
    assert!(available > 0);
}

#[test]
fn probing_a_missing_path_reports_nothing() {
    assert_eq!(
        DiskBudget::Probe.available(Path::new("./absent-budget-probe-path")),
        None
    );
}
