use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let aidl_dir = PathBuf::from("aidl/vendor/oplus/hardware/charger");

    println!("cargo:rerun-if-changed=aidl/vendor/oplus/hardware/charger/ICharger.aidl");

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_file = PathBuf::from(out_dir).join("charger.rs");

    rsbinder_aidl::Builder::new()
        .source(aidl_dir.join("ICharger.aidl"))
        .output(out_file.clone())
        .generate()
        .expect("Failed to generate AIDL bindings for ICharger");

    let generated =
        fs::read_to_string(&out_file).expect("Failed to read generated ICharger bindings");
    let mut version_code_patched = false;
    let mut hash_code_patched = false;
    let generated = generated
        .lines()
        .map(|line| {
            if line.contains("const r#getInterfaceVersion:") {
                version_code_patched = true;
                "                        pub(crate) const r#getInterfaceVersion: rsbinder::TransactionCode = 16777215;".to_string()
            } else if line.contains("const r#getInterfaceHash:") {
                hash_code_patched = true;
                "                        pub(crate) const r#getInterfaceHash: rsbinder::TransactionCode = 16777214;".to_string()
            } else if line.contains("#![allow(non_upper_case_globals, non_snake_case, dead_code)]") {
                line.replace("dead_code", "dead_code, non_camel_case_types")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        version_code_patched && hash_code_patched,
        "Generated AIDL bindings no longer contain interface metadata transaction constants"
    );
    fs::write(out_file, generated).expect("Failed to patch generated ICharger transaction codes");
}
