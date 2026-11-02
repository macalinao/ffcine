//! The footers of the long `--help`: examples per command, and the file formats that otherwise
//! live only in `docs/`.

/// `Examples:` and one indented line per example (`# ...` lines are comments).
macro_rules! examples {
    ($($line:literal),+ $(,)?) => {
        concat!("\n\nExamples:\n", $("\n      ", $line),+)
    };
}

pub const SNAP: &str = examples!(
    "# Move a rough 2.7 s - 7.3 s cut onto the nearest word edges",
    "ffcine snap A001_C003 2.7 7.3",
    "ffcine snap --keep --pad 0.2 A001_C003 1:02.5 1:09",
);

#[cfg(test)]
mod tests {
    /// The `--help` of `ffcine ARGS` as markdown (unwrapped), full or brief (`-h`).
    fn help(args: &[&str], full: bool) -> String {
        let mut words = args.to_vec();
        words.extend(["--help", "--help"]);
        match crate::cmd().run_inner(&words[..]) {
            Err(bpaf::ParseFailure::Stdout(doc, _)) => doc.render_markdown(full),
            other => panic!("ffcine {args:?} --help: {other:?}"),
        }
    }

    /// Commands and their one-line descriptions from the top-level help.
    fn commands() -> Vec<(String, String)> {
        let md = help(&[], true);
        let mut lines = md.lines().skip_while(|l| *l != "**Available commands:**");
        let mut out = Vec::new();
        while let Some(l) = lines.next() {
            if let Some(name) = l.strip_prefix("- **`").and_then(|l| l.split('`').next()) {
                out.push((
                    name.to_owned(),
                    lines.next().unwrap_or("").trim().to_owned(),
                ));
            }
        }
        out
    }

    #[test]
    fn long_help_flag() {
        let words = |v: &[&str]| {
            v.iter()
                .map(Into::into)
                .collect::<Vec<std::ffi::OsString>>()
        };
        assert_eq!(
            crate::long_help(words(&["grade", "--help"])),
            words(&["grade", "--help", "--help"])
        );
        assert_eq!(crate::long_help(words(&["-h"])), words(&["-h"]));
        assert_eq!(
            crate::long_help(words(&["publish", "--", "--help"])),
            words(&["publish", "--", "--help"])
        );
    }

    #[test]
    fn command_list_fits_one_line_each() {
        let cmds = commands();
        assert!(cmds.len() > 10, "{cmds:?}");
        for (name, descr) in &cmds {
            // bpaf lists only the first line of a description: it must be a sentence that fits.
            assert!(
                descr.ends_with('.') && descr.len() <= 80,
                "{name}: the first doc line should be one short sentence: {descr:?}"
            );
        }
    }

    /// Every command: every flag and positional has a help line, there are examples, and the full
    /// help says more than the brief one.
    #[test]
    fn every_command_is_documented() {
        let mut problems = Vec::new();
        for (name, _) in commands() {
            let md = help(&[&name], true);
            let lines: Vec<&str> = md.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                if l.starts_with("- **") || l.starts_with("- _") {
                    let next = lines.get(i + 1).copied().unwrap_or_default();
                    let text = next.trim();
                    if !next.starts_with("  ") || text.is_empty() || text.starts_with("[default") {
                        problems.push(format!("{name}: no help for {l}"));
                    }
                }
            }
            let usage = lines
                .iter()
                .find(|l| l.starts_with("**Usage**"))
                .unwrap_or(&"");
            for word in usage.split_whitespace() {
                let word = word.trim_start_matches("\\[");
                if let Some(meta) = word.strip_prefix("_`").and_then(|w| w.split('`').next()) {
                    if !md.contains(&format!("- _`{meta}`_")) {
                        problems.push(format!("{name}: no help for positional {meta}"));
                    }
                }
            }
            let examples = md.split_once("Examples:").map_or("", |(_, e)| e);
            if !examples.contains(&format!("ffcine {name}")) {
                problems.push(format!("{name}: no examples"));
            }
            if help(&[&name], false).len() >= md.len() {
                problems.push(format!("{name}: --help says no more than -h"));
            }
        }
        assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    }
}
