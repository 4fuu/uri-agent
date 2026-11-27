use anyhow::{Result, bail};
use std::fmt::Write as _;

const DOCUMENTS: &[(&str, &str)] = &[
    ("README.md", include_str!("../../docs/README.md")),
    (
        "configuration.md",
        include_str!("../../docs/configuration.md"),
    ),
    ("context.md", include_str!("../../docs/context.md")),
    ("development.md", include_str!("../../docs/development.md")),
    ("tools.md", include_str!("../../docs/tools.md")),
    ("release.md", include_str!("../../docs/release.md")),
    ("sessions.md", include_str!("../../docs/sessions.md")),
];

/// Render the `uat docs` output: the topic listing when `topic` is
/// `None`, or the named document's content.
pub fn docs_output(topic: Option<&str>) -> Result<String> {
    let Some(topic) = topic else {
        let mut output = String::from(
            "Documentation embedded in this binary; print one with `uat docs <topic>`:\n\n",
        );
        for (name, _) in DOCUMENTS {
            let _ = writeln!(output, "  {name}");
        }
        let _ = writeln!(output, "\n`README.md` is the index.");
        return Ok(output);
    };
    match DOCUMENTS.iter().find(|(name, _)| *name == topic) {
        Some((_, content)) => Ok((*content).to_string()),
        None => bail!(
            "unknown documentation topic: {topic}; run `uat docs` with no topic to list the exact filenames"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_output_lists_every_embedded_topic() {
        let listing = docs_output(None).unwrap();
        for (name, _) in DOCUMENTS {
            assert!(listing.contains(name), "missing {name} from listing");
        }
    }

    #[test]
    fn docs_output_prints_one_document_without_modification() {
        let document = docs_output(Some("configuration.md")).unwrap();
        assert!(document.contains("Configuration"));
    }

    #[test]
    fn docs_output_rejects_unknown_topics() {
        let error = docs_output(Some("missing.md")).unwrap_err();
        assert!(error.to_string().contains("unknown documentation topic"));
    }
}
