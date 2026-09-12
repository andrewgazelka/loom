use std::process::Command;

fn main() {
    let compiler = std::env::var_os("RUSTC").expect("Cargo must set RUSTC");
    let version = Command::new(&compiler)
        .arg("-vV")
        .output()
        .expect("rustc version");
    assert!(version.status.success(), "rustc -vV failed");
    let version_path =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("rustc-version.txt");
    std::fs::write(&version_path, &version.stdout).expect("write pinned version");
    println!(
        "cargo:rustc-env=HASH_RUSTC_VERSION_FILE={}",
        version_path.display()
    );
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
