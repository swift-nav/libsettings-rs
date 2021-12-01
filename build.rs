use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir: PathBuf = env::var("OUT_DIR").expect("OUT_DIR was not set").into();
    let settings =
        fs::read_to_string("src/libsettings/settings.yaml").expect("failed to load settings");
    fs::write(out_dir.join("settings.yaml"), settings).expect("failed to write settings");
}
