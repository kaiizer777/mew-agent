// Phase 4 (Bug 3 fix): verify the on-disk transcript lands in
// the directory the caller specified, and that a caller passing
// `None` keeps the historical default behavior.
//
// This is the most direct test of the Bug 3 fix without needing
// the Tauri dev server to be running. We pass two different
// `transcript_dir` values to two `Agent::new` calls and confirm:
//   1. A file is created in the caller-specified dir.
//   2. A file is NOT created in `src-tauri/transcripts/` (the
//      path that triggered the Bug 3 watcher restart loop).
//   3. The default (`None`) still writes to `./transcripts/`
//      under the cwd so the existing CLI behavior is unchanged.
//
// We do NOT call `agent.run()` (that would need a browser +
// LLM). We just call `Agent::new` with a tiny task, then
// immediately drop the Agent. The file is created at
// construction time (see the "Record the initial Start
// transition" block in mew-agent/src/agent.rs), so a fresh
// session is enough to exercise the path.

use mew_agent::agent::Agent;
use mew_agent::load_config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Use a per-run temp dir so we don't pollute the repo.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let out_root = std::path::PathBuf::from("tests-output")
        .join("phase4_bug3_path")
        .join(format!("run_{}", stamp));
    let _ = std::fs::create_dir_all(&out_root);

    let custom_dir = out_root.join("custom_transcripts");
    let _ = std::fs::create_dir_all(&custom_dir);

    let config = load_config()?;

    // 1. Caller-specified dir: file MUST land there.
    println!("[bug3] test 1: caller-specified transcript dir");
    let agent1 = Agent::new(
        config.clone(),
        "test task (custom dir)",
        Some(custom_dir.clone()),
    );
    drop(agent1);
    let s1 = agent1_session_id_via_dir(&custom_dir);
    match s1 {
        Some(sid) => println!("[bug3] PASS: transcript file created in {} (session={})", custom_dir.display(), sid),
        None => {
            println!("[bug3] FAIL: no transcript file found in {}", custom_dir.display());
            std::process::exit(1);
        }
    }

    // 2. Confirm no transcript file landed under src-tauri/.
    //    The path is relative to cwd; we check by walking the
    //    expected location.
    println!("\n[bug3] test 2: NO transcript file under src-tauri/");
    let tauri_dir = std::path::PathBuf::from("mew-ui").join("src-tauri");
    let src_transcripts = tauri_dir.join("transcripts");
    let mut found_in_src = 0;
    if src_transcripts.exists() {
        if let Ok(rd) = std::fs::read_dir(&src_transcripts) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // We only care about files created in this run
                // (the same session_id we just generated).
                if name.starts_with("transcript_session_") {
                    found_in_src += 1;
                }
            }
        }
    }
    if found_in_src == 0 {
        println!("[bug3] PASS: zero transcript files under {}/ (this run)", src_transcripts.display());
    } else {
        // The dir may legitimately have OLD files from prior
        // pre-fix runs. We only care about files created
        // *now*. Since we just made a new session_id at
        // `stamp`, the pre-fix files won't have that exact
        // session_id.
        // Look for any file with our current session_id.
        let our_id_files: Vec<String> = match std::fs::read_dir(&src_transcripts) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.contains(&stamp.to_string()))
                .collect(),
            Err(_) => Vec::new(),
        };
        if our_id_files.is_empty() {
            println!("[bug3] PASS: zero transcript files under {} for this session ({} prior files exist from old runs)", src_transcripts.display(), found_in_src);
        } else {
            println!("[bug3] FAIL: transcript file landed under {} this run: {:?}", src_transcripts.display(), our_id_files);
            std::process::exit(1);
        }
    }

    // 3. Default (None): should land in ./transcripts/ under cwd.
    println!("\n[bug3] test 3: default (None) writes to ./transcripts/");
    let default_dir = std::path::PathBuf::from("transcripts");
    let before = count_files_with_session_id(&default_dir, &stamp.to_string());
    let agent2 = Agent::new(config, "test task (default dir)", None);
    drop(agent2);
    let after = count_files_with_session_id(&default_dir, &stamp.to_string());
    if after > before {
        println!("[bug3] PASS: default behavior preserved (file landed in {}/)", default_dir.display());
    } else {
        println!("[bug3] FAIL: no file landed in {} for default (None) call", default_dir.display());
        std::process::exit(1);
    }

    println!("\n[bug3] ALL OK");
    Ok(())
}

fn agent1_session_id_via_dir(dir: &std::path::Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("transcript_session_") && name.ends_with(".log") {
            return Some(name);
        }
    }
    None
}

fn count_files_with_session_id(dir: &std::path::Path, stamp: &str) -> usize {
    if !dir.exists() {
        return 0;
    }
    match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(stamp))
            .count(),
        Err(_) => 0,
    }
}
