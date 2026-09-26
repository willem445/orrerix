// PLANT (#3547 review scratch): a nested command file carrying an unbarriered sync command.
#[tauri::command]
pub fn plant_nested_bare_sync(reg: tauri::State<Arc<OrchRegistry>>) -> bool {
    let _ = &reg;
    false
}
