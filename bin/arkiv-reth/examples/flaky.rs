use dev_flaky::FlakyRun;

fn main() {
    let result = FlakyRun::new("arkiv-reth", "0.2.1")
        .in_dir("bin/arkiv-reth")
        .iterations(20)
        .execute()
        .unwrap();

    println!(
        "stable={} flaky={} broken={}",
        result.stable_count(),
        result.flaky_count(),
        result.broken_count()
    );
    println!("{}", result.into_report().to_json().unwrap());
}
