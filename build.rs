include!("version.rs");

fn main() {
    set_version();
    // Templates and static files are compiled in; rebuild when they change.
    println!("cargo:rerun-if-changed=templates");
    println!("cargo:rerun-if-changed=static");
    println!("cargo:rerun-if-changed=migrations");
}
