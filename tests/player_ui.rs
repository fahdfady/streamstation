use std::path::Path;
use std::process::Command;

#[test]
fn player_attaches_once_per_station_index() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = manifest.join("tests").join("player.test.mjs");

    match Command::new("node").arg(&script).current_dir(manifest).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "player UI test failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            );
            print!("{stdout}");
        }
        Err(err) => {
            eprintln!("skipping player UI test: node is not runnable here ({err})");
        }
    }
}
