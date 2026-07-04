mod common;

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_fish_integration_test() {
    if !docker_available() {
        eprintln!("skipping fish integration test: docker not available");
        return;
    }

    common::run_bake_target("fish-integration-test").expect("fish integration test failed");
    println!("Successfully tested fish integration with flyline");
}

#[test]
fn fish_integration_test() {
    run_fish_integration_test();
}
