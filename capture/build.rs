//! Stamps the Windows version resource onto clipper-capture.exe.
//!
//! Without it Task Manager has nothing to print but the filename, so an always-on recorder shows
//! up in somebody's process list as an anonymous `clipper-capture.exe` with a blank Description
//! column — exactly the shape of a thing you kill on sight. `FileDescription` is what Task
//! Manager's Name column shows for a child process, so that string is the one that matters.
//!
//! The icon is the app's, shared rather than copied: one file to change when the logo changes.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../assets/icon.ico");

    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../assets/icon.ico");
    res.set("FileDescription", "Clipper Instant Replay");
    res.set("ProductName", "Clipper");
    res.set("OriginalFilename", "clipper-capture.exe");
    res.set("InternalName", "clipper-capture");
    res.set("CompanyName", "Dmytro Hissa");
    res.set("LegalCopyright", "Copyright \u{a9} 2026 Dmytro Hissa");

    // Compiling a resource needs rc.exe from the Windows SDK. A machine without one can still
    // build a working recorder, so this warns rather than failing the build — but the binary it
    // produces is the nameless one described above, which is not what should ship.
    if let Err(e) = res.compile() {
        println!("cargo:warning=no version resource ({e}); clipper-capture.exe will be unnamed in Task Manager");
    }
}
