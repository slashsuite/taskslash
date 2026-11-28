//! Whether the tests should see the scope path: asked of the systemd user manager itself, not the
//! way the daemon probes, so a broken probe or broken scope arguments fail the tests instead of
//! quietly turning every run plain.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Set (CI sets it) to make a missing user session a failure rather than a skip.
const REQUIRE: &str = "TASKSLASH_REQUIRE_SCOPE";

/// `true` when this machine has a systemd user session, so the daemon's default launch must be
/// `scope`. Without one it says so once on stderr and returns `false`, unless `TASKSLASH_REQUIRE_SCOPE`
/// is set, when it panics.
pub fn scope_expected() -> bool {
    static EXPECTED: OnceLock<bool> = OnceLock::new();
    *EXPECTED.get_or_init(|| {
        let session = Command::new("systemctl")
            .args(["--user", "show", "--property=Version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !session {
            assert!(
                std::env::var_os(REQUIRE).is_none(),
                "{REQUIRE} is set but systemctl --user reaches no user manager"
            );
            // Straight to stderr: the test harness captures eprintln! and shows it only on a failure.
            let _ = writeln!(
                std::io::stderr(),
                "no systemd user session: the scope path is not tested here, only the plain one"
            );
        }
        session
    })
}

/// The launch path a run must record: `launch` when the test forced one, else this machine's.
pub fn expected_launch(launch: Option<&str>) -> &str {
    launch.unwrap_or(if scope_expected() { "scope" } else { "plain" })
}
