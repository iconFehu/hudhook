use std::path::PathBuf;

use hudhook::inject::Process;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_default();
    let name = args.next().ok_or_else(|| {
        format!("Usage: {} <process.exe> <dll path> [callback export]", program.to_string_lossy())
    })?;
    let dll: PathBuf = args.next().ok_or("Missing DLL path")?.into();
    let callback = args.next().unwrap_or_else(|| "L4D2_RequestResume".into());
    if args.next().is_some() {
        return Err("Unexpected extra arguments".into());
    }
    let name = name.to_str().ok_or("Process name must be Unicode")?;
    let callback = callback.to_str().ok_or("Callback export must be Unicode")?;
    let process = Process::by_name(name)?;
    let outcome = process.inject_with_optional_callback(dll, callback)?;
    println!("{}", if outcome.newly_loaded { "DLL loaded." } else { "Reused the loaded DLL." });
    if outcome.callback_invoked {
        println!("{callback}: request accepted; completion is handled by the DLL.");
    } else {
        println!("Optional callback {callback} was not found.");
    }
    Ok(())
}
