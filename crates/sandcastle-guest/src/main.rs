//! Static helper executed by libkrun's init inside every sandcastle microVM.

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod copy;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod layer;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod user;

#[cfg(target_os = "linux")]
mod commit;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod overlay;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod unpack;

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandcastle-guest only runs inside a sandcastle microVM");
    std::process::exit(125);
}
