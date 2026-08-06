// Probe: launch stealth Chrome, start the screencast, navigate the page,
// observe how many frames arrive on the channel. Reproduces the
// "1 frame received" bug to see if it's a chromiumoxide event-bus
// issue or something else.
use std::time::{Duration, Instant};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let binary_path = Some(
        "c:\\Users\\bari2\\Desktop\\mew-agent\\stealth-browser\\chrome.exe".to_string(),
    );
    println!("[probe] launching headless Chrome...");
    let launch = tokio::time::timeout(
        Duration::from_secs(20),
        mew_cdp::launch_headless(binary_path, false),
    )
    .await;
    let (mut browser, page, _handler, _job) = match launch {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            println!("[probe] launch failed: {e:?}");
            return;
        }
        Err(_) => {
            println!("[probe] launch timeout");
            return;
        }
    };
    println!("[probe] launch OK");

    // Pre-navigate to a data: URL — same as the Tauri shell.
    let loading_html = "data:text/html;charset=utf-8,\
<!doctype html><html><head><title>loading</title></head>\
<body style='margin:0;height:100vh;background:linear-gradient(180deg,#f8fafc,#eef2ff);\
display:flex;align-items:center;justify-content:center;color:#475569;\
font-family:sans-serif'>loading...</body></html>";
    let nav = chromiumoxide::cdp::browser_protocol::page::NavigateParams::builder()
        .url(loading_html)
        .build()
        .unwrap();
    let _ = page.execute(nav).await;
    println!("[probe] pre-navigated to data: loading page");

    // Set up the channel + screencast.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, i32)>();
    let started = mew_cdp::start_screencast_with_first_frame(&page, tx).await;
    println!("[probe] start_screencast_with_first_frame: {started:?}");

    // Spawn a counter task that prints each frame.
    let counter = tokio::spawn(async move {
        let mut n = 0u32;
        let start = Instant::now();
        loop {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Some((data, w))) => {
                    n += 1;
                    println!(
                        "[probe]   frame #{n}  bytes={}  device_w={w}  t={:?}",
                        data.len(),
                        start.elapsed()
                    );
                }
                Ok(None) => {
                    println!("[probe]   channel closed");
                    break;
                }
                Err(_) => {
                    if start.elapsed() > Duration::from_secs(20) {
                        println!("[probe]   giving up after 20s");
                        break;
                    }
                }
            }
        }
        n
    });

    // Wait 2s for the one-shot.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Now navigate the page to a real URL — simulating the agent's
    // first navigation. The poll-based screencast should fire frames.
    println!("[probe] navigating to https://example.com ...");
    let nav2 = chromiumoxide::cdp::browser_protocol::page::NavigateParams::builder()
        .url("https://example.com")
        .build()
        .unwrap();
    let _ = page.execute(nav2).await;
    println!("[probe] navigate sent");

    // Wait for ~5s of polling (10 polls at 500ms).
    tokio::time::sleep(Duration::from_secs(5)).await;

    println!("[probe] closing browser");
    let _ = browser.close().await;
    let _ = counter.await;
}
