use std::process::Command;

fn main() {
    let compiler = std::env::var_os("RUSTC").expect("Cargo must set RUSTC");
    let output = Command::new(compiler)
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc sysroot");
    assert!(output.status.success(), "rustc --print sysroot failed");
    let sysroot = String::from_utf8(output.stdout).expect("UTF-8 sysroot");
    println!("cargo:rustc-env=HASH_RUSTC_SYSROOT={}", sysroot.trim());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}/lib", sysroot.trim());
    println!("cargo:rerun-if-changed=rust-toolchain.toml");
}
