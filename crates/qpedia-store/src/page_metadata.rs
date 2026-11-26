use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde_yaml::Mapping;
use crate::wikirepo::WikiRepo;

const FIELDS: [&str; 3] = ["id", "created_at", "updated_at"];

pub(crate) fn split(page: &str) -> Result<(&str, &str)> {
    let mut lines = page.trim_start().split_inclusive('\n');
    if lines.next().is_none_or(|line| line.trim_end() != "---") {
        return Ok(("", page));
    }
    let start = page.trim_start().find('\n').context("unterminated page frontmatter")? + 1;
    let rest = &page.trim_start()[start..];
    let mut end = 0;
    for line in lines {
        if line.trim_end() == "---" {
            return Ok((&rest[..end], &rest[end + line.len()..]));
        }
        end += line.len();
    }
    anyhow::bail!("unterminated page frontmatter");
}

/// Source IDs used for both reader authorization and the current filing context.
pub fn page_source_ids(content: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Provenance {
        #[serde(default)]
        source_ids: Vec<String>,
    }
    let (header, _) = split(content)?;
    if header.is_empty() { return Ok(vec![]); }
    Ok(serde_yaml::from_str::<Provenance>(header)?.source_ids)
}

fn values(header: &str) -> Result<[Option<String>; 3]> {
    let map: Mapping = serde_yaml::from_str(header).context("invalid page frontmatter")?;
    let mut result = [None, None, None];
    for (i, key) in FIELDS.iter().enumerate() {
        if let Some(value) = map.get(*key) {
            let value = value.as_str().context("managed page field must be a string")?;
            ensure!(!value.is_empty() && !value.contains(['\n','\r']), "empty or multiline managed page field");
            result[i] = Some(value.to_owned());
        }
    }
    Ok(result)
}

fn managed(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) { return false; }
    line.split_once(':').is_some_and(|(key, _)| FIELDS.contains(&key.trim().trim_matches(['\'', '"'])))
}

pub(crate) fn is_managed(path: &str) -> bool {
    !qpedia_core::wiki::is_system_page(path)
}

impl WikiRepo {
    pub(crate) async fn stamp_page(&self, path: &str, proposed: &str, previous: Option<&str>) -> Result<String> {
        let (header, body) = split(proposed)?;
        let mut trusted = [None, None, None];
        let mut first_date = None;
        let mut last_date = None;
        if let Some(existing) = previous {
            trusted = values(split(existing)?.0)?;
            if trusted.iter().any(Option::is_none) {
                // Stop at this path's last creation, even if an older page used its name.
                let history = self.git(&["log", "--first-parent", "--format=%H %cI", "--name-status", "--", path]).await?;
                let mut commits = history.lines().filter(|line| !line.is_empty());
                while let (Some(head), Some(status)) = (commits.next(), commits.next()) {
                    let (sha, when) = head.split_once(' ').context("invalid Git history")?;
                    if status.starts_with("D\t") { break; }
                    first_date = Some(when.to_owned());
                    last_date.get_or_insert_with(|| when.to_owned());
                    let page = self.git(&["show", &format!("{sha}:{path}")]).await?;
                    let old = values(split(&page)?.0)?;
                    for (current, prior) in trusted.iter_mut().zip(old) {
                        if current.is_none() { *current = prior; }
                    }
                    if trusted.iter().all(Option::is_some) || status.starts_with("A\t") { break; }
                }
            }
        }
        let now = Utc::now().to_rfc3339();
        let fields = [
            trusted[0].take().unwrap_or_else(|| ulid::Ulid::new().to_string()),
            trusted[1].take().or(first_date).unwrap_or_else(|| now.clone()),
            trusted[2].take().or(last_date).unwrap_or(now),
        ];
        // Preserve other YAML formatting, including inline lists used by the search index.
        let mut rebuilt = String::from("---\n");
        for line in header.split_inclusive('\n').filter(|line| !managed(line)) {
            rebuilt.push_str(line);
        }
        for (key, value) in FIELDS.iter().zip(&fields) {
            let scalar = if value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_:.+".contains(&b)) {
                value.clone()
            } else { serde_json::to_string(value)? };
            rebuilt.push_str(&format!("{key}: {scalar}\n"));
        }
        rebuilt.push_str("---\n");
        rebuilt.push_str(body);
        ensure!(values(split(&rebuilt)?.0)? == fields.map(Some), "conflicting managed page metadata");
        Ok(rebuilt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qpedia_core::wiki::{DiffBundle, DiffOp};

    #[tokio::test]
    async fn replacements_preserve_identity_and_restore_lost_fields_from_git() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = WikiRepo::open_or_init(temp.path(), "test", "test@example.com").await?;
        let path = "topics/example.md";
        let original = "---\ntitle: Example\nkind: concept\ntags: [one, two]\n---\nfirst\n";
        let bundle = |op| DiffBundle { ingest_id: "test".into(), operations: vec![op], summary: String::new() };
        repo.commit_bundle(&bundle(DiffOp::Create {path:path.into(), content:original.into(), rationale:String::new()})).await?;
        let created = repo.read_page(path).await?.unwrap();
        let trusted = values(split(&created)?.0)?;
        assert!(trusted.iter().all(Option::is_some));
        tokio::fs::write(temp.path().join(path), original).await?;
        repo.git(&["commit", "-qam", "legacy replacement stripped the fields"]).await?;
        let altered = "---\nkind: concept\ntitle: Example\ntags: [one, two]\n'id': counterfeit\ncreated_at: guessed\nupdated_at: guessed\n---\nreplacement\nid: body line\n";
        repo.commit_bundle(&bundle(DiffOp::Patch {path:path.into(), new_content:altered.into(), rationale:String::new()})).await?;
        let recovered = repo.read_page(path).await?.unwrap();
        assert_eq!(values(split(&recovered)?.0)?, trusted);
        assert!(recovered.contains("tags: [one, two]") && recovered.ends_with("id: body line\n"));
        assert_eq!(repo.stamp_page(path, &recovered, Some(&recovered)).await?, recovered);
        assert!(!repo.read_page("index.md").await?.unwrap().starts_with("---"));
        Ok(())
    }
}
