use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

fn long_option(token: &str) -> Option<&str> {
    let name = token
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .next()?;
    name.strip_prefix("--")
        .filter(|name| !name.is_empty())
        .map(|_| name)
}

#[test]
fn readme_options_match_binary_help() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let readme =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut readme_lines = readme
        .lines()
        .skip_while(|line| line.trim() != "## Options");
    assert!(readme_lines.next().is_some(), "README.md has no ## Options");
    let documented: BTreeSet<_> = readme_lines
        .take_while(|line| !line.trim_start().starts_with("## "))
        .filter_map(|line| {
            let row = line.trim().strip_prefix('|')?;
            let (first_column, _) = row.split_once('|')?;
            let code = first_column.trim().strip_prefix('`')?;
            let (option, _) = code.split_once('`')?;
            long_option(option)
        })
        .collect();

    let output = Command::new(env!("CARGO_BIN_EXE_comic-auto-resize"))
        .arg("--help")
        .output()
        .expect("runs the binary");
    assert!(
        output.status.success(),
        "--help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).expect("--help is UTF-8");
    let mut help_lines = help.lines().skip_while(|line| line.trim() != "Options:");
    assert!(help_lines.next().is_some(), "--help has no Options section");
    let implemented: BTreeSet<_> = help_lines
        .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            // Clap indents declarations by two columns, with four more for a missing short flag.
            // Wrapped descriptions are indented further and must not become declarations.
            if !line.starts_with("  -") && !line.starts_with("      --") {
                return None;
            }
            let declaration = line.trim_start().split("  ").next()?;
            declaration.split_whitespace().find_map(long_option)
        })
        .collect();
    assert!(!implemented.is_empty(), "--help declares no long options");
    assert_eq!(
        documented,
        implemented,
        "README.md options differ from --help; missing: {:?}; invented: {:?}",
        implemented.difference(&documented).collect::<Vec<_>>(),
        documented.difference(&implemented).collect::<Vec<_>>()
    );
}
