//! Load test: many senders throw a seeded, varied stream of transactions at a
//! live node, while a watcher reads every block, keeps a ledger of where each
//! transaction is, and checks invariants as it goes. The run stops at the first
//! broken invariant, and every run writes a report that alone says whether it
//! was clean. See the README's "Load test".
//!
//! Both tests are ignored: they run by hand, outside CI.

mod ledger;
mod model;
mod plan;
mod report;
mod rng;
mod run;

use run::{Settings, run};

/// The load run, sized by `ARKIV_LOAD_TXS` and `ARKIV_LOAD_SENDERS`, replayed
/// with `ARKIV_LOAD_SEED`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a load test, run by hand: see the README"]
async fn load() {
    let result = run(Settings::from_env()).await;
    assert_eq!(
        result.outcome(),
        "passed",
        "violations: {}\nreport: {}",
        result.report["violations"],
        result.dir.join("report.json").display()
    );
}

/// The watcher's self-check: a transaction recorded as sent but never sent
/// fails the run with a `lost` violation naming it, long before any timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a load test, run by hand: see the README"]
async fn a_lost_transaction_fails_the_run() {
    let result = run(Settings {
        txs: 60,
        senders: 4,
        phantom: true,
        ..Settings::from_env()
    })
    .await;
    let report = result.dir.join("report.json");
    assert_eq!(result.outcome(), "failed", "report: {}", report.display());
    let violations = result.report["violations"].as_array().unwrap();
    let phantom = result.report["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tx| tx["phantom"] == true)
        .expect("the phantom is in the report")["hash"]
        .clone();
    assert!(
        violations
            .iter()
            .any(|v| v["kind"] == "lost" && v["txs"].as_array().unwrap().contains(&phantom)),
        "a lost violation names the phantom {phantom}: {violations:?}, report: {}",
        report.display()
    );
    assert_eq!(result.report["summary"]["lost"], 1, "only the phantom");
}
