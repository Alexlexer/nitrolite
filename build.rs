//! Embed the application icon and version metadata into the executable.
//!
//! Without this the binary shows the generic Windows icon in Explorer, and the
//! tray falls back to the stock application icon.

fn main() {
    println!("cargo:rerun-if-changed=assets/nitrolite.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/nitrolite.ico");
    res.set("ProductName", "nitrolite");
    res.set(
        "FileDescription",
        "Minimal replacement for Acer NitroSense: direct ACPI/WMI control",
    );
    res.set("OriginalFilename", "nitrolite.exe");
    res.set("LegalCopyright", "MIT licensed");
    res.set("ProductVersion", env!("CARGO_PKG_VERSION"));

    if let Err(e) = res.compile() {
        // Do not fail the build for a cosmetic resource: a missing icon must
        // never stop the tool from working.
        println!("cargo:warning=resource compile failed: {e}");
    }
}
