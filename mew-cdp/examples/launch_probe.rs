// Reproduces the Tauri app's launch path: resolves profile dir via the
// production helper, runs the reaper, and calls launch_headless with the
// same binary path. Used to debug the "Lock file can not be created"
// failure that the user is hitting.
use std::time::Duration;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let binary_path =
            Some("c:\\Users\\bari2\\Desktop\\mew-agent\\stealth-browser\\chrome.exe".to_string());
        let visible_cursor = false;

        println!("[probe] calling mew_cdp::launch_headless...");
        let start = std::time::Instant::now();
        let res = mew_cdp::launch_headless(binary_path, visible_cursor).await;
        println!(
            "[probe] launch_headless returned after {:?}: {:?}",
            start.elapsed(),
            res.is_ok()
        );

        if let Err(e) = res {
            println!("[probe] ERROR: {e:?}");
            eprintln!("[probe] ERROR chain:");
            let mut src: Option<&dyn std::error::Error> = e.source();
            let mut depth = 1;
            while let Some(s) = src {
                eprintln!("[probe]   {depth}: {s}");
                src = s.source();
                depth += 1;
            }
        } else {
            // Hold the browser for a moment so we can observe the lockfile.
            println!("[probe] launch succeeded, holding for 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
