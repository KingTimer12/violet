//! Gives the Windows executable its icon and version details, which
//! Explorer, the taskbar and the Alt+Tab switcher show. Other systems take
//! the icon from the package (macOS's bundle, Linux's desktop entry).

fn main() {
    println!("cargo:rerun-if-changed=assets/app/violet.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/app/violet.ico")
        .set("ProductName", "Violet")
        .set("FileDescription", "Violet")
        .set("CompanyName", "Violet");
    // A release must not go out without its icon: fail rather than warn.
    if let Err(error) = resource.compile() {
        panic!("Could not embed the Windows icon (needs rc.exe from the Windows SDK): {error}");
    }
}
