// Included by the build scripts of the binaries: sets APP_VERSION for `--version`.
// Release builds have their version written into Cargo.toml (scripts/set-version.sh). Anything
// else is 0.0.0-dev there and gets its version from the git tags instead, like
// scripts/version.sh: 0.4.1-dev.5+gde73aff is five commits after v0.4.0.

fn set_version() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap();
    let version = if pkg == "0.0.0-dev" { git_version().unwrap_or(pkg) } else { pkg };
    println!("cargo:rustc-env=APP_VERSION={version}");
}

fn git(args: &[&str]) -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::process::Command::new("git").args(args).current_dir(manifest_dir).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_version() -> Option<String> {
    // Only this repository's history: a source tarball unpacked inside some other repository
    // (an AUR package's, say) isn't tracked there and must not take that one's tags.
    git(&["ls-files", "--error-unmatch", "Cargo.toml"])?;
    // Rebuild when HEAD moves or a tag is added. Only paths that exist: cargo reruns the
    // build script every time for a missing one.
    let git_dir = git(&["rev-parse", "--absolute-git-dir"])?;
    let common_dir = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let mut watched = vec![format!("{git_dir}/HEAD"), format!("{common_dir}/refs/tags"), format!("{common_dir}/packed-refs")];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(format!("{common_dir}/{branch}"));
    }
    for path in watched.iter().filter(|p| std::path::Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={path}");
    }

    let hash = git(&["rev-parse", "--short=7", "HEAD"])?;
    let Some(described) = git(&["describe", "--tags", "--match", "v[0-9]*", "--long", "--abbrev=7"]) else {
        return Some(format!("0.0.0-dev+g{hash}"));
    };
    // v0.4.0-5-gde73aff (the tag itself may contain dashes: v0.5.0-rc.1-5-gde73aff)
    let mut parts = described.rsplitn(3, '-');
    let (_, height, tag) = (parts.next()?, parts.next()?, parts.next()?);
    let version = tag.strip_prefix('v')?;
    Some(match height {
        "0" => version.to_owned(),
        _ if version.contains('-') => format!("{version}.{height}+g{hash}"),
        _ => {
            let (major_minor, patch) = version.rsplit_once('.')?;
            format!("{major_minor}.{}-dev.{height}+g{hash}", patch.parse::<u64>().ok()? + 1)
        }
    })
}
