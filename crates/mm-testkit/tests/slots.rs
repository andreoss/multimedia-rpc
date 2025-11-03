use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn concurrent_holders_never_exceed_the_slot_count() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let active = active.clone();
        let peak = peak.clone();
        handles.push(thread::spawn(move || {
            let _slot = mm_testkit::stack_slot();
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(50));
            active.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for handle in handles {
        handle.join().expect("holder finishes");
    }
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak >= 1, "no holder ever ran");
    assert!(
        peak <= mm_testkit::STACK_SLOTS as usize,
        "{peak} concurrent holders exceeded the bound"
    );
}

#[test]
fn a_dropped_slot_frees_its_lock_for_the_next_holder() {
    let first = mm_testkit::stack_slot();
    drop(first);
    let again = mm_testkit::stack_slot();
    drop(again);
}
