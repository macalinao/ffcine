//! On-screen text through the `tesseract` CLI, when it is installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `tesseract` from `PATH`, if it runs.
pub fn find() -> Option<PathBuf> {
    let ok = Command::new("tesseract")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then(|| PathBuf::from("tesseract"))
}

/// Text in `image` (sparse-text mode), cleaned of recognition noise; empty if none or on error.
pub fn read(tesseract: &Path, image: &Path) -> String {
    let Ok(out) = Command::new(tesseract)
        .arg(image)
        .args(["stdout", "--psm", "11"])
        .stderr(Stdio::null())
        .output()
    else {
        return String::new();
    };
    clean(&String::from_utf8_lossy(&out.stdout))
}

/// Keeps the lines that look like words: mostly letters and digits, with a word of 3+ letters.
pub fn clean(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| {
            let chars = l.chars().filter(|c| !c.is_whitespace()).count();
            let alnum = l.chars().filter(|c| c.is_alphanumeric()).count();
            chars > 0
                && alnum * 10 >= chars * 7
                && l.split_whitespace()
                    .any(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3)
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn drops_noise() {
        assert_eq!(
            super::clean("  | ~ .\nDAY 3 in Kyoto\n\n,.;\nee\nbest ramen!\n"),
            "DAY 3 in Kyoto / best ramen!"
        );
    }
}
