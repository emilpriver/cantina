use crate::errors;

pub async fn install(package_name: &String) -> errors::Result<()> {
    println!("Installing task: {}", package_name);

    Ok(())
}
