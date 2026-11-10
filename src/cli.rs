use std::path::PathBuf;

/// What `kotoview <arg>` on the command line should open, resolved
/// once at startup from `std::env::args()`. Relative paths (including `.`)
/// are resolved against the shell's cwd.
pub enum InitialTarget {
    Workspace(PathBuf),
    Archive(PathBuf),
}

/// `Path::canonicalize()` on Windows returns the `\\?\`-prefixed verbatim
/// form (e.g. `\\?\D:\photos`). It's fine for `std::fs`, but not the kind of
/// path a user wants to see in a title bar, so strip it back to normal form.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) => match s.strip_prefix(r"\\?\") {
            Some(stripped) => PathBuf::from(stripped),
            None => path,
        },
        None => path,
    }
}

pub fn resolve_initial_target() -> Option<InitialTarget> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = args.into_iter().find(|a| !a.starts_with('-'))?;
    let cwd = std::env::current_dir().ok()?;
    let resolved = cwd.join(&arg);
    let canonical = strip_verbatim_prefix(resolved.canonicalize().unwrap_or(resolved));
    if canonical.is_dir() {
        Some(InitialTarget::Workspace(canonical))
    } else if canonical.is_file()
        && canonical
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("zip"))
            .unwrap_or(false)
    {
        Some(InitialTarget::Archive(canonical))
    } else {
        None
    }
}
