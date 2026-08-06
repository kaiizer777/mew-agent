fn main() {
    if let Ok(p) = std::env::var("MEW_BROWSER_PROFILE") {
        println!("env-var: {}", p);
    } else {
        println!("no env-var");
    }
    let exe = std::env::current_exe().ok();
    println!("current_exe: {:?}", exe);
    let parent = exe.as_ref().and_then(|p| p.parent());
    println!("parent:     {:?}", parent);
    if let Some(p) = parent {
        println!("would resolve to: {}", p.join("profile").display());
    }
    let cwd = std::env::current_dir().ok();
    println!("current_dir: {:?}", cwd);
    if let Some(c) = cwd {
        println!("cwd-relative would be: {}", c.join("profile").display());
    }
}
