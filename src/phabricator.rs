use crate::types::{Comment, CommentLine, DiffResponse, LineType, ReviewAction, ReviewNote, Side};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Deserialize)]
struct ConduitResponse<T> {
    error_code: Option<String>,
    error_info: Option<String>,
    result: Option<T>,
}

impl<T> ConduitResponse<T> {
    fn into_result(self) -> Result<T> {
        if let Some(error_code) = self.error_code {
            anyhow::bail!(
                "Phabricator API error {}: {}",
                error_code,
                self.error_info.unwrap_or_default()
            );
        }
        self.result
            .context("Phabricator response did not include result")
    }
}

#[derive(Debug, Deserialize)]
struct RevisionSearchData {
    data: Vec<RevisionData>,
}

#[derive(Debug, Deserialize)]
struct RevisionData {
    phid: String,
    fields: RevisionFields,
}

#[derive(Debug, Deserialize)]
struct RevisionFields {
    uri: Option<String>,
    #[serde(rename = "diffPHID")]
    diff_phid: Option<String>,
    #[serde(rename = "authorPHID")]
    author_phid: Option<String>,
    status: Option<RevisionStatus>,
}

#[derive(Debug, Deserialize)]
struct RevisionStatus {
    #[serde(default)]
    closed: bool,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TransactionSearchData {
    data: Vec<TransactionData>,
    cursor: Option<SearchCursor>,
}

#[derive(Debug, Deserialize)]
struct SearchCursor {
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TransactionData {
    #[serde(rename = "type")]
    transaction_type: Option<String>,
    #[serde(rename = "authorPHID")]
    author_phid: Option<String>,
    fields: Option<serde_json::Value>,
    comments: Vec<CommentData>,
}

#[derive(Debug, Deserialize)]
struct CommentData {
    id: serde_json::Value,
    #[serde(rename = "authorPHID")]
    author_phid: Option<String>,
    removed: Option<bool>,
    content: CommentContent,
}

#[derive(Debug, Deserialize)]
struct CommentContent {
    raw: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserSearchData {
    data: Vec<UserData>,
}

#[derive(Debug, Deserialize)]
struct UserData {
    phid: String,
    fields: UserFields,
}

#[derive(Debug, Deserialize)]
struct UserFields {
    #[serde(rename = "realName")]
    real_name: Option<String>,
    username: Option<String>,
}

/// Result of `phid.query`: the display name of any object, e.g. the Herald application.
#[derive(Debug, Deserialize)]
struct PhidInfo {
    name: Option<String>,
}

#[derive(Clone)]
pub struct PhabricatorClient {
    base_url: String,
    token: String,
    client: Client,
}

impl PhabricatorClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("lrv/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to create Phabricator HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }

    pub async fn fetch_review_notes(
        &self,
        revision: &str,
        include_done: bool,
    ) -> Result<Vec<ReviewNote>> {
        let revision_id = parse_revision_id(revision).context("Invalid Phabricator revision")?;
        let revision = self.fetch_revision(revision_id).await?;
        let transactions = self.fetch_transactions(&revision.phid).await?;
        let users = self.fetch_user_names(&transactions).await;

        let mut notes = Vec::new();
        for transaction in transactions {
            match transaction.transaction_type.as_deref() {
                Some("inline") => {
                    self.append_inline_notes(
                        &mut notes,
                        &revision,
                        &transaction,
                        &users,
                        include_done,
                    );
                }
                Some("comment") => {
                    self.append_revision_notes(&mut notes, &revision, &transaction, &users);
                }
                _ => {}
            }
        }

        notes.sort_by_key(|note| {
            (
                note.commit_idx.unwrap_or(usize::MAX),
                note.file.clone(),
                note_line_start(&note.line),
            )
        });
        Ok(notes)
    }

    async fn fetch_revision(&self, revision_id: u32) -> Result<RevisionData> {
        let url = format!("{}/api/differential.revision.search", self.base_url);
        let params = vec![
            ("api.token".to_string(), self.token.clone()),
            ("constraints[ids][0]".to_string(), revision_id.to_string()),
        ];
        let response = self
            .client
            .post(url)
            .form(&params)
            .send()
            .await
            .context("Failed to fetch Phabricator revision")?;
        let response = response
            .json::<ConduitResponse<RevisionSearchData>>()
            .await
            .context("Failed to parse Phabricator revision response")?
            .into_result()?;
        response
            .data
            .into_iter()
            .next()
            .with_context(|| format!("No Phabricator revision found for D{}", revision_id))
    }

    async fn fetch_transactions(&self, object_phid: &str) -> Result<Vec<TransactionData>> {
        let mut after: Option<String> = None;
        let mut all = Vec::new();

        loop {
            let url = format!("{}/api/transaction.search", self.base_url);
            let mut params = vec![
                ("api.token".to_string(), self.token.clone()),
                ("objectIdentifier".to_string(), object_phid.to_string()),
            ];
            if let Some(cursor) = after.as_ref() {
                params.push(("after".to_string(), cursor.clone()));
            }
            let response = self
                .client
                .post(url)
                .form(&params)
                .send()
                .await
                .context("Failed to fetch Phabricator transactions")?;
            let response = response
                .json::<ConduitResponse<TransactionSearchData>>()
                .await
                .context("Failed to parse Phabricator transaction response")?
                .into_result()?;
            all.extend(response.data);
            after = response.cursor.and_then(|cursor| cursor.after);
            if after.is_none() {
                break;
            }
        }

        Ok(all)
    }

    async fn fetch_user_names(&self, transactions: &[TransactionData]) -> HashMap<String, String> {
        let mut phids = HashSet::new();
        for transaction in transactions {
            if let Some(phid) = transaction.author_phid.as_ref() {
                phids.insert(phid.clone());
            }
            for comment in &transaction.comments {
                if let Some(phid) = comment.author_phid.as_ref() {
                    phids.insert(phid.clone());
                }
            }
        }
        if phids.is_empty() {
            return HashMap::new();
        }

        let url = format!("{}/api/user.search", self.base_url);
        let mut params = vec![("api.token".to_string(), self.token.clone())];
        for (idx, phid) in phids.iter().enumerate() {
            params.push((format!("constraints[phids][{}]", idx), phid.clone()));
        }

        let Ok(response) = self.client.post(url).form(&params).send().await else {
            return HashMap::new();
        };
        let Ok(response) = response.json::<ConduitResponse<UserSearchData>>().await else {
            return HashMap::new();
        };
        let Ok(data) = response.into_result() else {
            return HashMap::new();
        };

        let mut users = HashMap::new();
        for user in data.data {
            let display_name = display_user_name(&user.fields).unwrap_or_else(|| user.phid.clone());
            users.insert(user.phid, display_name);
        }
        // Authors that aren't users, such as Herald rules, only resolve through `phid.query`.
        let unresolved: Vec<&String> = phids.iter().filter(|p| !users.contains_key(*p)).collect();
        users.extend(self.fetch_phid_names(&unresolved).await);
        users
    }

    async fn fetch_phid_names(&self, phids: &[&String]) -> HashMap<String, String> {
        if phids.is_empty() {
            return HashMap::new();
        }
        let url = format!("{}/api/phid.query", self.base_url);
        let mut params = vec![("api.token".to_string(), self.token.clone())];
        for (idx, phid) in phids.iter().enumerate() {
            params.push((format!("phids[{}]", idx), (*phid).clone()));
        }
        let Ok(response) = self.client.post(url).form(&params).send().await else {
            return HashMap::new();
        };
        let Ok(response) = response
            .json::<ConduitResponse<HashMap<String, PhidInfo>>>()
            .await
        else {
            return HashMap::new();
        };
        let Ok(infos) = response.into_result() else {
            return HashMap::new();
        };
        infos
            .into_iter()
            .filter_map(|(phid, info)| Some((phid, info.name.filter(|n| !n.is_empty())?)))
            .collect()
    }

    fn append_inline_notes(
        &self,
        notes: &mut Vec<ReviewNote>,
        revision: &RevisionData,
        transaction: &TransactionData,
        users: &HashMap<String, String>,
        include_done: bool,
    ) {
        let fields = transaction
            .fields
            .as_ref()
            .unwrap_or(&serde_json::Value::Null);
        let is_done = fields
            .get("isDone")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if is_done && !include_done {
            return;
        }

        let Some(file_path) = fields.get("path").and_then(|value| value.as_str()) else {
            return;
        };
        let Some(line) = fields.get("line").and_then(|value| value.as_u64()) else {
            return;
        };
        if line == 0 {
            return;
        }
        let length = fields
            .get("length")
            .and_then(|value| value.as_u64())
            .unwrap_or(1)
            .max(1);
        let side = if fields.get("isNewFile").and_then(|v| v.as_bool()) == Some(false) {
            Side::Old
        } else {
            Side::New
        };
        let line = line as usize;
        let line = if length > 1 {
            CommentLine::Range((line, line + length as usize - 1))
        } else {
            CommentLine::Single(line)
        };

        for comment in &transaction.comments {
            if comment.removed.unwrap_or(false) {
                continue;
            }
            let body = comment.content.raw.as_deref().unwrap_or("").trim();
            if body.is_empty() {
                continue;
            }
            let author_phid = comment
                .author_phid
                .as_ref()
                .or(transaction.author_phid.as_ref());
            notes.push(ReviewNote {
                id: Some(comment.id.to_string()),
                file: file_path.to_string(),
                line: line.clone(),
                side,
                body: body.to_string(),
                author: author_phid.and_then(|phid| users.get(phid).cloned()),
                date: None,
                source_url: revision.fields.uri.clone(),
                commit_idx: None,
                read_only: false,
            });
        }
    }

    fn append_revision_notes(
        &self,
        notes: &mut Vec<ReviewNote>,
        revision: &RevisionData,
        transaction: &TransactionData,
        users: &HashMap<String, String>,
    ) {
        for comment in &transaction.comments {
            if comment.removed.unwrap_or(false) {
                continue;
            }
            let body = comment.content.raw.as_deref().unwrap_or("").trim();
            if body.is_empty() {
                continue;
            }
            let author_phid = comment
                .author_phid
                .as_ref()
                .or(transaction.author_phid.as_ref());
            notes.push(ReviewNote::overall(
                Some(comment.id.to_string()),
                body.to_string(),
                author_phid.and_then(|phid| users.get(phid).cloned()),
                None,
                revision.fields.uri.clone(),
                None,
            ));
        }
    }
}

/// The user the API token belongs to.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Viewer {
    pub phid: String,
    pub username: String,
}

/// A revision's active diff, as far as it is needed to check that a local commit matches it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveDiff {
    pub revision_phid: String,
    pub uri: String,
    pub author_phid: Option<String>,
    /// Closed or abandoned revisions can still be commented on, but rarely should be.
    pub closed: bool,
    pub diff_id: u64,
    /// Lines added and deleted per path.
    pub files: HashMap<String, (usize, usize)>,
}

/// A draft inline comment: `length` is the number of lines it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct InlineDraft {
    pub file: String,
    pub is_new_file: bool,
    pub line: usize,
    pub length: usize,
    pub body: String,
}

/// Everything to publish on one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct RevisionPublish {
    pub commit_idx: usize,
    pub inlines: Vec<InlineDraft>,
    pub comment: Option<String>,
    pub action: ReviewAction,
}

/// How far publishing a revision got, so that retrying after a failure doesn't repeat it.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct PublishProgress {
    pub inlines_created: usize,
    pub published: bool,
}

/// Split a review into what to publish on each commit's revision. File comments become inlines;
/// `(commit)` comments become the revision's general comment, and the overall comment goes on the
/// last commit's. `action` applies to every revision. A revision with nothing to say is left out
/// unless the action is a verdict.
#[allow(dead_code)]
pub fn build_publish_plan(
    comments: &[Comment],
    overall_comment: Option<&str>,
    action: ReviewAction,
    commit_count: usize,
) -> Vec<RevisionPublish> {
    let mut plan: Vec<RevisionPublish> = (0..commit_count)
        .map(|commit_idx| RevisionPublish {
            commit_idx,
            inlines: Vec::new(),
            comment: None,
            action,
        })
        .collect();
    let mut general: Vec<Vec<&str>> = vec![Vec::new(); commit_count];

    for comment in comments {
        let idx = comment
            .commit_idx
            .unwrap_or(0)
            .min(commit_count.saturating_sub(1));
        let Some(revision) = plan.get_mut(idx) else {
            continue;
        };
        if comment.file == "(commit)" {
            general[idx].push(&comment.body);
            continue;
        }
        let (line, length) = match &comment.line {
            CommentLine::Single(line) => (*line, 1),
            CommentLine::Range((start, end)) => (*start, end - start + 1),
        };
        revision.inlines.push(InlineDraft {
            file: comment.file.clone(),
            is_new_file: comment.side == Side::New,
            line,
            length,
            body: comment.body.clone(),
        });
    }
    if let (Some(overall), Some(last)) = (overall_comment, general.last_mut()) {
        if !overall.trim().is_empty() {
            last.push(overall);
        }
    }
    for (revision, general) in plan.iter_mut().zip(general) {
        if !general.is_empty() {
            revision.comment = Some(general.join("\n\n"));
        }
    }
    plan.retain(|r| {
        !r.inlines.is_empty() || r.comment.is_some() || r.action != ReviewAction::Comment
    });
    plan
}

/// Per-path lines added and deleted of a local commit, to compare with a revision's diff.
#[allow(dead_code)]
pub fn local_file_stats(diff: &DiffResponse) -> HashMap<String, (usize, usize)> {
    diff.files
        .iter()
        .map(|file| {
            let lines = file.hunks.iter().flat_map(|h| &h.lines);
            let (mut added, mut deleted) = (0, 0);
            for line in lines {
                match line.line_type {
                    LineType::Add => added += 1,
                    LineType::Delete => deleted += 1,
                    LineType::Context => {}
                }
            }
            (file.path.clone(), (added, deleted))
        })
        .collect()
}

/// Inline comments are anchored to lines of the revision's diff, so publishing from a commit that
/// differs from it would put them on the wrong lines. Binary files have no line counts to compare.
#[allow(dead_code)]
pub fn check_diff_matches(
    revision: &str,
    local: &HashMap<String, (usize, usize)>,
    active: &ActiveDiff,
) -> Result<()> {
    let mut mismatched: Vec<&str> = local
        .iter()
        .filter(|(path, stats)| active.files.get(*path) != Some(stats))
        .map(|(path, _)| path.as_str())
        .chain(
            active
                .files
                .keys()
                .filter(|path| !local.contains_key(*path))
                .map(String::as_str),
        )
        .collect();
    if mismatched.is_empty() {
        return Ok(());
    }
    mismatched.sort_unstable();
    mismatched.dedup();
    anyhow::bail!(
        "the local commit doesn't match the latest diff (id {}) of {}; these files differ: {}",
        active.diff_id,
        revision,
        mismatched.join(", ")
    )
}

impl PhabricatorClient {
    async fn conduit<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
    ) -> Result<T> {
        params.push(("api.token".to_string(), self.token.clone()));
        self.client
            .post(format!("{}/api/{}", self.base_url, method))
            .form(&params)
            .send()
            .await
            .with_context(|| format!("Phabricator {method} request failed"))?
            .json::<ConduitResponse<T>>()
            .await
            .with_context(|| format!("Failed to parse Phabricator {method} response"))?
            .into_result()
    }

    /// The user the token belongs to; also proves the token works.
    #[allow(dead_code)]
    pub async fn whoami(&self) -> Result<Viewer> {
        let value: serde_json::Value = self.conduit("user.whoami", Vec::new()).await?;
        let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
        Ok(Viewer {
            phid: text("phid").context("user.whoami returned no phid")?,
            username: text("userName").context("user.whoami returned no userName")?,
        })
    }

    /// The revision and the files of its latest diff.
    #[allow(dead_code)]
    pub async fn fetch_active_diff(&self, revision: u32) -> Result<ActiveDiff> {
        let revision = self.fetch_revision(revision).await?;
        let diff_phid = revision
            .fields
            .diff_phid
            .clone()
            .context("Phabricator revision has no diff")?;

        let search: serde_json::Value = self
            .conduit(
                "differential.diff.search",
                vec![("constraints[phids][0]".to_string(), diff_phid)],
            )
            .await?;
        let diff_id = search
            .pointer("/data/0/id")
            .and_then(|v| v.as_u64())
            .context("Phabricator diff not found")?;

        let queried: serde_json::Value = self
            .conduit(
                "differential.querydiffs",
                vec![("ids[0]".to_string(), diff_id.to_string())],
            )
            .await?;
        let changes = queried
            .get(diff_id.to_string())
            .and_then(|d| d.get("changes"))
            .and_then(|c| c.as_array())
            .context("Phabricator diff has no changes")?;
        // Line counts arrive as strings.
        let count = |change: &serde_json::Value, key: &str| {
            change.get(key).map_or(0, |v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0) as usize
            })
        };
        let files = changes
            .iter()
            .filter_map(|change| {
                let path = change.get("currentPath")?.as_str()?.to_string();
                Some((path, (count(change, "addLines"), count(change, "delLines"))))
            })
            .collect();

        let status = revision.fields.status.as_ref();
        Ok(ActiveDiff {
            revision_phid: revision.phid,
            uri: revision.fields.uri.unwrap_or_default(),
            author_phid: revision.fields.author_phid,
            closed: status.is_some_and(|s| {
                s.closed || matches!(s.value.as_deref(), Some("abandoned" | "changes-planned"))
            }),
            diff_id,
            files,
        })
    }

    async fn create_inline(&self, diff_id: u64, inline: &InlineDraft) -> Result<()> {
        let _: serde_json::Value = self
            .conduit(
                "differential.createinline",
                vec![
                    ("diffID".to_string(), diff_id.to_string()),
                    ("filePath".to_string(), inline.file.clone()),
                    // Phabricator wants 1/0; `true` is read as false.
                    (
                        "isNewFile".to_string(),
                        u8::from(inline.is_new_file).to_string(),
                    ),
                    ("lineNumber".to_string(), inline.line.to_string()),
                    // The number of lines after the first.
                    ("lineLength".to_string(), (inline.length - 1).to_string()),
                    ("content".to_string(), inline.body.clone()),
                ],
            )
            .await?;
        Ok(())
    }

    /// Publish a revision's review: create its inline comments as drafts, then post the comment
    /// (and verdict) with the drafts attached. `progress` records how far it got, so that calling
    /// this again after an error continues instead of creating the inlines twice.
    #[allow(dead_code)]
    pub async fn publish_revision(
        &self,
        revision: u32,
        diff_id: u64,
        publish: &RevisionPublish,
        progress: &mut PublishProgress,
    ) -> Result<()> {
        if progress.published {
            return Ok(());
        }
        for inline in publish.inlines.iter().skip(progress.inlines_created) {
            self.create_inline(diff_id, inline).await?;
            progress.inlines_created += 1;
        }
        let mut params = vec![
            ("revision_id".to_string(), revision.to_string()),
            (
                "message".to_string(),
                publish.comment.clone().unwrap_or_default(),
            ),
            ("attach_inlines".to_string(), "1".to_string()),
        ];
        match publish.action {
            ReviewAction::Comment => {}
            ReviewAction::Accept => params.push(("action".to_string(), "accept".to_string())),
            ReviewAction::RequestChanges => {
                params.push(("action".to_string(), "reject".to_string()))
            }
        }
        let _: serde_json::Value = self.conduit("differential.createcomment", params).await?;
        progress.published = true;
        Ok(())
    }
}

/// Find the `Differential Revision: <url>` trailer that moz-phab and arc add to
/// commit messages. Returns the Phabricator base URL and the revision id, so the
/// host comes from the commit itself rather than from configuration.
pub fn detect_revision(commit_message: &str) -> Option<(String, u32)> {
    const TRAILER: &str = "differential revision:";
    let url = commit_message.lines().rev().find_map(|line| {
        let line = line.trim();
        let head = line.get(..TRAILER.len())?;
        head.eq_ignore_ascii_case(TRAILER)
            .then(|| line[TRAILER.len()..].trim())
    })?;
    let (base, _) = url.rsplit_once("/D")?;
    Some((base.to_string(), parse_revision_id(url)?))
}

/// A Phabricator revision a commit is reviewed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhabTarget {
    pub base_url: String,
    pub id: u32,
}

/// The revision of each commit: the `named` revision at the same position when given, else the
/// commit's `Differential Revision:` trailer. `None` for commits with neither.
pub fn resolve_phab_targets(
    named: &[String],
    base_url: &str,
    commit_messages: &[Option<&str>],
) -> Vec<Option<PhabTarget>> {
    commit_messages
        .iter()
        .enumerate()
        .map(|(i, message)| {
            if let Some(id) = named.get(i).and_then(|r| parse_revision_id(r)) {
                return Some(PhabTarget {
                    base_url: base_url.to_string(),
                    id,
                });
            }
            let (base_url, id) = detect_revision((*message)?)?;
            Some(PhabTarget { base_url, id })
        })
        .collect()
}

fn parse_revision_id(input: &str) -> Option<u32> {
    let trimmed = input.trim();
    if let Some((_, tail)) = trimmed.rsplit_once("/D") {
        return tail
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok();
    }
    trimmed
        .trim_start_matches('D')
        .trim_start_matches('d')
        .parse()
        .ok()
}

fn display_user_name(fields: &UserFields) -> Option<String> {
    match (fields.real_name.as_deref(), fields.username.as_deref()) {
        (Some(real), Some(username)) if !real.is_empty() && !username.is_empty() => {
            Some(format!("{} ({})", real, username))
        }
        (Some(real), _) if !real.is_empty() => Some(real.to_string()),
        (_, Some(username)) if !username.is_empty() => Some(username.to_string()),
        _ => None,
    }
}

fn note_line_start(line: &CommentLine) -> usize {
    match line {
        CommentLine::Single(line) => *line,
        CommentLine::Range((start, _)) => *start,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_publish_plan, check_diff_matches, detect_revision, parse_revision_id,
        resolve_phab_targets, ActiveDiff, InlineDraft, PhabTarget,
    };
    use crate::types::{Comment, CommentLine, ReviewAction, Side};
    use std::collections::HashMap;

    fn comment(file: &str, line: CommentLine, side: Side, commit_idx: Option<usize>) -> Comment {
        Comment {
            file: file.to_string(),
            line,
            side,
            body: format!("body of {file}"),
            commit_idx,
        }
    }

    #[test]
    fn detects_revision_trailer() {
        let msg = "Bug 1 - Fix it r=reviewer\n\nDifferential Revision: https://phabricator.services.mozilla.com/D301379\n";
        assert_eq!(
            detect_revision(msg),
            Some((
                "https://phabricator.services.mozilla.com".to_string(),
                301379
            ))
        );
        assert_eq!(detect_revision("Bug 1 - no trailer"), None);
        assert_eq!(
            detect_revision("Differential Revision: https://example.com/not-a-revision"),
            None
        );
    }

    #[test]
    fn parse_revision_ids() {
        assert_eq!(parse_revision_id("D301379"), Some(301379));
        assert_eq!(parse_revision_id("301379"), Some(301379));
        assert_eq!(
            parse_revision_id("https://phabricator.services.mozilla.com/D301379"),
            Some(301379)
        );
        assert_eq!(parse_revision_id("Dnope"), None);
    }

    #[test]
    fn resolves_named_then_trailer_targets() {
        let trailer = "Bug 1\n\nDifferential Revision: https://phab.example/D7\n";
        let target = |base: &str, id| {
            Some(PhabTarget {
                base_url: base.to_string(),
                id,
            })
        };
        let targets = resolve_phab_targets(
            &["D5".to_string()],
            "https://named.example",
            &[Some("Bug 0"), Some(trailer), None],
        );
        assert_eq!(
            targets,
            vec![
                target("https://named.example", 5),
                target("https://phab.example", 7),
                None
            ]
        );
    }

    #[test]
    fn plan_groups_comments_per_commit() {
        let comments = vec![
            comment("a.rs", CommentLine::Single(3), Side::New, Some(0)),
            comment("b.rs", CommentLine::Range((5, 8)), Side::Old, Some(1)),
            comment("(commit)", CommentLine::Single(1), Side::New, Some(1)),
        ];
        let plan = build_publish_plan(&comments, Some("overall"), ReviewAction::Comment, 2);
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].inlines,
            vec![InlineDraft {
                file: "a.rs".to_string(),
                is_new_file: true,
                line: 3,
                length: 1,
                body: "body of a.rs".to_string(),
            }]
        );
        assert_eq!(plan[0].comment, None);
        assert_eq!((plan[1].inlines[0].line, plan[1].inlines[0].length), (5, 4));
        assert!(!plan[1].inlines[0].is_new_file);
        // The commit-message comment and the overall comment share the last revision.
        assert_eq!(
            plan[1].comment.as_deref(),
            Some("body of (commit)\n\noverall")
        );
    }

    #[test]
    fn plan_pins_unindexed_comments_to_the_first_commit_and_skips_empty_revisions() {
        let comments = vec![comment("a.rs", CommentLine::Single(1), Side::New, None)];
        let plan = build_publish_plan(&comments, None, ReviewAction::Comment, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].commit_idx, 0);
    }

    #[test]
    fn plan_keeps_empty_revisions_for_a_verdict() {
        let plan = build_publish_plan(&[], None, ReviewAction::Accept, 2);
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|r| r.action == ReviewAction::Accept));
        assert!(build_publish_plan(&[], Some("  "), ReviewAction::Comment, 2).is_empty());
    }

    #[test]
    fn diff_match_reports_differing_files() {
        let active = ActiveDiff {
            revision_phid: String::new(),
            uri: String::new(),
            author_phid: None,
            closed: false,
            diff_id: 9,
            files: HashMap::from([("a.rs".to_string(), (2, 1)), ("b.rs".to_string(), (1, 1))]),
        };
        let mut local = HashMap::from([("a.rs".to_string(), (2, 1)), ("b.rs".to_string(), (1, 1))]);
        assert!(check_diff_matches("D1", &local, &active).is_ok());

        local.insert("a.rs".to_string(), (3, 1));
        local.remove("b.rs");
        local.insert("c.rs".to_string(), (1, 0));
        let err = check_diff_matches("D1", &local, &active)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a.rs, b.rs, c.rs"), "{err}");
        assert!(err.contains("id 9"), "{err}");
    }
}
