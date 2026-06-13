use crate::usecase::doctor::check_dependencies;

pub fn run() -> anyhow::Result<()> {
    for check in check_dependencies() {
        let mark = if check.available { "ok" } else { "missing" };
        println!("{:<10} {}", check.name, mark);
    }
    Ok(())
}
