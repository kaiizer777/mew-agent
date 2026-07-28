use chromiumoxide::Page;
async fn test(page: &Page) {
    let _ = page.press_key("Enter").await;
}
