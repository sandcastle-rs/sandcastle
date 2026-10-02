//! Static helper executed by libkrun's init inside every sandcastle microVM.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod probe;

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandcastle-guest only runs inside a sandcastle microVM");
    std::process::exit(125);
}
