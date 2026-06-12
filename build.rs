//! Build script with one job: hand the linker our linker script — properly.
//!
//! This used to be a `rustflags = ["-C", "link-arg=-T..."]` line in
//! .cargo/config.toml. That route has two real flaws, one of which bit us:
//!
//! 1. **Cargo never learns the script is a build input.** A -T smuggled in
//!    through rustflags shows up in no dep-info file, so
//!    `touch linker.ld && cargo build` says "Finished" and relinks nothing.
//!    Any edit to the memory layout (load address, section order, stack
//!    size) silently ships the *old* layout until an unrelated .rs change
//!    forces a rebuild — precisely the "boots fine, wrong layout" failure
//!    mode docs/01-boot-flow.md warns about. `rerun-if-changed` makes the
//!    dependency explicit and the relink automatic.
//! 2. **A relative -T path resolves against your cwd**, so the build only
//!    worked when cargo was invoked from the repo root. CARGO_MANIFEST_DIR
//!    pins it to this directory regardless of where you run cargo.
//!
//! (Bonus: editing `rustflags` invalidates the entire build cache; link
//! args emitted here do not.)

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{manifest_dir}/src/arch/aarch64/linker.ld");

    // Relink whenever the memory layout changes...
    println!("cargo::rerun-if-changed={script}");
    // ...and pass the script to the linker when building the kernel.
    println!("cargo::rustc-link-arg=-T{script}");
}
