// Phase 1.2 evidence: prove mew-agent is genuinely linked into the mew-ui
// binary's dep graph by calling `load_config` from a binary that uses the
// same workspace. If this prints real config values, the linkage is real
// and `get_config_summary` in src-tauri will also see the same values at
// runtime.
use mew_agent::{load_config, OpencodeZenConfig};

fn main() -> anyhow::Result<()> {
    let cfg = load_config()?;
    let zen: &OpencodeZenConfig = &cfg.opencode_zen;
    println!("Phase 1.2 linkage proof");
    println!("  default_model = {}", zen.default_model);
    println!("  base_url      = {}", zen.base_url);
    println!("  max_iter      = {}", zen.max_iterations);
    println!(
        "  api_key_len   = {}",
        zen.api_key.len()
    );
    if let Some(b) = &cfg.browser {
        println!("  browser.bin   = {:?}", b.binary_path);
        println!("  visible_cur   = {}", b.visible_cursor);
    } else {
        println!("  browser       = (none in config)");
    }
    Ok(())
}
