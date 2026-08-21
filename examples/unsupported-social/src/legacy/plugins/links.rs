use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::{
    FormData, MAX_BODY_LEN, MAX_COMMENTS, MAX_LINKS, MAX_TITLE_LEN, MAX_URL_LEN, PagePlugin,
    extract_domain, format_time_ago, parse_form_data, sanitize_field, sanitize_user_content,
};

/// A submitted link in the aggregator.
#[derive(Clone)]
struct LinkEntry {
    id: u64,
    timestamp: u64,
    name: String,
    title: String,
    url: String,
    score: u64,
}

/// A comment on a link, supporting threading via parent_id.
#[derive(Clone)]
struct Comment {
    id: u64,
    link_id: u64,
    /// 0 means top-level comment.
    parent_id: u64,
    timestamp: u64,
    name: String,
    body: String,
    score: u64,
}

/// Storage for links and comments on a single page.
struct LinkStore {
    links: Vec<LinkEntry>,
    comments: Vec<Comment>,
    next_id: u64,
    links_path: PathBuf,
    comments_path: PathBuf,
    /// Persistent, authenticated vote dedup: (lowercase username, kind, id).
    votes: HashSet<(String, char, u64)>,
    votes_path: PathBuf,
}

impl LinkStore {
    fn load(links_path: PathBuf, comments_path: PathBuf) -> Self {
        let votes_path = links_path.with_extension("votes.tsv");
        let links: Vec<LinkEntry> = if links_path.exists() {
            std::fs::read_to_string(&links_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(6, '\t');
                    Some(LinkEntry {
                        id: parts.next()?.parse().ok()?,
                        timestamp: parts.next()?.parse().ok()?,
                        name: parts.next()?.to_string(),
                        title: parts.next()?.to_string(),
                        url: parts.next()?.to_string(),
                        score: parts.next()?.parse().ok()?,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let comments: Vec<Comment> = if comments_path.exists() {
            std::fs::read_to_string(&comments_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(7, '\t');
                    Some(Comment {
                        id: parts.next()?.parse().ok()?,
                        link_id: parts.next()?.parse().ok()?,
                        parent_id: parts.next()?.parse().ok()?,
                        timestamp: parts.next()?.parse().ok()?,
                        name: parts.next()?.to_string(),
                        body: parts.next()?.to_string(),
                        score: parts.next().and_then(|s| s.parse().ok()).unwrap_or(1),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let max_id = links
            .iter()
            .map(|l| l.id)
            .chain(comments.iter().map(|c| c.id))
            .max()
            .unwrap_or(0);

        let votes = std::fs::read_to_string(&votes_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let username = parts.next()?.to_string();
                let kind = parts.next()?.chars().next()?;
                let id = parts.next()?.parse().ok()?;
                matches!(kind, 'l' | 'c').then_some((username, kind, id))
            })
            .collect();

        LinkStore {
            links,
            comments,
            next_id: max_id + 1,
            links_path,
            comments_path,
            votes,
            votes_path,
        }
    }

    fn save_links(&self) -> Result<(), String> {
        let content: String = self
            .links
            .iter()
            .map(|l| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    l.id, l.timestamp, l.name, l.title, l.url, l.score
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        atomic_write(&self.links_path, &content)
    }

    fn save_comments(&self) -> Result<(), String> {
        let content: String = self
            .comments
            .iter()
            .map(|c| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    c.id, c.link_id, c.parent_id, c.timestamp, c.name, c.body, c.score
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        atomic_write(&self.comments_path, &content)
    }

    fn save_votes(&self) -> Result<(), String> {
        let mut records: Vec<_> = self.votes.iter().collect();
        records.sort();
        let content = records
            .into_iter()
            .map(|(name, kind, id)| format!("{name}\t{kind}\t{id}"))
            .collect::<Vec<_>>()
            .join("\n");
        atomic_write(&self.votes_path, &content)
    }

    fn add_link(&mut self, name: String, title: String, url: String) -> Result<u64, String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let id = self.next_id;
        let previous_links = self.links.clone();
        self.links.push(LinkEntry {
            id,
            timestamp,
            name,
            title,
            url,
            score: 1,
        });
        self.next_id += 1;

        // Trim oldest links if over limit
        if self.links.len() > MAX_LINKS {
            let excess = self.links.len() - MAX_LINKS;
            self.links.drain(..excess);
        }

        if let Err(error) = self.save_links() {
            self.links = previous_links;
            self.next_id = id;
            return Err(error);
        }
        Ok(id)
    }

    fn add_comment(
        &mut self,
        link_id: u64,
        parent_id: u64,
        name: String,
        body: String,
    ) -> Result<u64, String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let id = self.next_id;
        let previous_comments = self.comments.clone();
        self.comments.push(Comment {
            id,
            link_id,
            parent_id,
            timestamp,
            name,
            body,
            score: 1,
        });
        self.next_id += 1;

        // Trim oldest comments if over limit
        if self.comments.len() > MAX_COMMENTS {
            let excess = self.comments.len() - MAX_COMMENTS;
            self.comments.drain(..excess);
        }

        if let Err(error) = self.save_comments() {
            self.comments = previous_comments;
            self.next_id = id;
            return Err(error);
        }
        Ok(id)
    }

    fn upvote(&mut self, link_id: u64, username: &str) -> Result<bool, String> {
        let key = (username.to_lowercase(), 'l', link_id);
        if self.votes.contains(&key) {
            return Ok(false);
        }
        let Some(index) = self.links.iter().position(|l| l.id == link_id) else {
            return Err("Link not found.".into());
        };
        self.links[index].score += 1;
        self.votes.insert(key.clone());
        if let Err(error) = self.save_links().and_then(|_| self.save_votes()) {
            self.links[index].score -= 1;
            self.votes.remove(&key);
            let _ = self.save_links();
            let _ = self.save_votes();
            return Err(error);
        }
        Ok(true)
    }

    fn upvote_comment(&mut self, comment_id: u64, username: &str) -> Result<bool, String> {
        let key = (username.to_lowercase(), 'c', comment_id);
        if self.votes.contains(&key) {
            return Ok(false);
        }
        let Some(index) = self.comments.iter().position(|c| c.id == comment_id) else {
            return Err("Comment not found.".into());
        };
        self.comments[index].score += 1;
        self.votes.insert(key.clone());
        if let Err(error) = self.save_comments().and_then(|_| self.save_votes()) {
            self.comments[index].score -= 1;
            self.votes.remove(&key);
            let _ = self.save_comments();
            let _ = self.save_votes();
            return Err(error);
        }
        Ok(true)
    }

    fn comment_count(&self, link_id: u64) -> usize {
        self.comments
            .iter()
            .filter(|c| c.link_id == link_id)
            .count()
    }

    /// Render the front page: ranked list of links.
    fn render_front_page(&self, page_path: &str, logged_in: bool) -> String {
        if self.links.is_empty() {
            return "[text dim]No links yet. Be the first to submit![/text]\n".to_string();
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Sort by HN gravity: score / (age_hours + 2)^1.5
        let mut ranked: Vec<(usize, &LinkEntry)> = self.links.iter().enumerate().collect();
        ranked.sort_by(|(_, a), (_, b)| {
            let age_a = (now.saturating_sub(a.timestamp) as f64) / 3600.0;
            let age_b = (now.saturating_sub(b.timestamp) as f64) / 3600.0;
            let rank_a = a.score as f64 / (age_a + 2.0).powf(1.5);
            let rank_b = b.score as f64 / (age_b + 2.0).powf(1.5);
            rank_b
                .partial_cmp(&rank_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut out = String::new();
        for (rank, (_, link)) in ranked.iter().enumerate() {
            let time_ago = format_time_ago(link.timestamp);
            let domain = extract_domain(&link.url);
            let n_comments = self.comment_count(link.id);
            let title = sanitize_user_content(&link.title);
            let name = sanitize_user_content(&link.name);
            let domain_esc = sanitize_user_content(domain);
            let title_markup = if link.url.is_empty() {
                title
            } else {
                format!("[link href=\"{}\"]{title}[/link]", link.url)
            };
            let vote_markup = if logged_in {
                format!(
                    "[button action=submit target=\"{page_path}?vote={}\"]▲[/button]",
                    link.id,
                )
            } else {
                "[text dim]▲[/text]".to_string()
            };
            let domain_markup = if domain_esc.is_empty() {
                String::new()
            } else {
                format!(" [text dim]({domain_esc})[/text]")
            };

            let comments_label = match n_comments {
                0 => "discuss".to_string(),
                1 => "1 comment".to_string(),
                n => format!("{n} comments"),
            };

            // Line 1: rank. ▲ Title (domain)
            out.push_str(&format!(
                "[text][text dim]{num}.[/text] {vote_markup} {title_markup}{domain_markup}[/text]\n",
                num = rank + 1,
                vote_markup = vote_markup,
                title_markup = title_markup,
                domain_markup = domain_markup,
            ));
            // Line 2: points by user time ago | comments
            out.push_str(&format!(
                "[text]   [text dim]{score} points by {name} {time_ago} |[/text] [link href=\"{page_path}?item={id}\"][text fg=white]{comments_label}[/text][/link][/text]\n[spacer lines=1 /]\n",
                score = link.score,
                name = name,
                time_ago = time_ago,
                comments_label = comments_label,
                id = link.id,
            ));
        }
        out
    }

    /// Render the item/discussion page for a single link.
    fn render_item_page(
        &self,
        link_id: u64,
        reply_to: Option<u64>,
        logged_in: bool,
        page_path: &str,
    ) -> String {
        let link = match self.links.iter().find(|l| l.id == link_id) {
            Some(l) => l,
            None => return "[text dim]Link not found.[/text]\n".to_string(),
        };

        let mut out = String::new();
        let title = sanitize_user_content(&link.title);
        let name = sanitize_user_content(&link.name);
        let domain = sanitize_user_content(extract_domain(&link.url));
        let time_ago = format_time_ago(link.timestamp);
        let title_markup = if link.url.is_empty() {
            format!("[text bold]{title}[/text]")
        } else {
            format!(
                "[link href=\"{}\"][text bold]{title}[/text][/link]",
                link.url
            )
        };
        let domain_markup = if domain.is_empty() {
            String::new()
        } else {
            format!(" [text dim]({domain})[/text]")
        };
        let vote_markup = if logged_in {
            format!(
                "[button action=submit target=\"{page_path}?vote={}\"]▲ upvote[/button] [text dim]|[/text] ",
                link.id,
            )
        } else {
            String::new()
        };

        // Link header
        out.push_str(&format!(
            "[text]{title_markup}{domain_markup}[/text]\n\
             [text][text dim]{score} points by {name} {time_ago} |[/text] {vote_markup}[link href=\"{page_path}\"][text fg=white]back[/text][/link][/text]\n\
             [hr style=single fg=white /]\n\
             [spacer lines=1 /]\n",
            title_markup = title_markup,
            domain_markup = domain_markup,
            vote_markup = vote_markup,
            score = link.score,
            name = name,
            time_ago = time_ago,
            page_path = page_path,
        ));

        // Render threaded comments
        let link_comments: Vec<&Comment> = self
            .comments
            .iter()
            .filter(|c| c.link_id == link_id)
            .collect();

        if link_comments.is_empty() && reply_to.is_none() {
            out.push_str("[text dim]No comments yet.[/text]\n");
        } else {
            out.push_str(&self.render_comment_tree(
                &link_comments,
                link_id,
                0,
                0,
                reply_to,
                logged_in,
                page_path,
            ));
        }

        // Comment form / login link at the bottom
        if reply_to.is_none() {
            out.push_str("[spacer lines=1 /]\n[hr style=single fg=white /]\n[spacer lines=1 /]\n");
            if logged_in {
                out.push_str(&self.render_comment_form(link_id, 0, page_path));
            } else {
                out.push_str(
                    "[link href=\"/login\"][text fg=white]log in to comment[/text][/link]\n",
                );
            }
        }

        out
    }

    /// Render a comment form box.
    fn render_comment_form(&self, link_id: u64, parent_id: u64, page_path: &str) -> String {
        let action = if parent_id == 0 {
            format!("{page_path}?item={link_id}")
        } else {
            format!("{page_path}?item={link_id}&parent={parent_id}")
        };
        let label = if parent_id == 0 {
            "Add a Comment"
        } else {
            "Reply"
        };
        format!(
            "[form action=\"{action}\"]\n\
             [text dim]{label}[/text]\n\
             [input name=\"msg\" placeholder=\"\" maxlen=500 /]\n\
             [button action=submit]{label_lower}[/button]\n\
             [/form]\n\
             [spacer lines=1 /]\n",
            label = label,
            action = action,
            label_lower = label.to_lowercase(),
        )
    }

    /// Recursively render the comment tree using collapsible `[details]` elements.
    #[allow(clippy::too_many_arguments)]
    fn render_comment_tree(
        &self,
        all_comments: &[&Comment],
        link_id: u64,
        parent_id: u64,
        depth: usize,
        reply_to: Option<u64>,
        logged_in: bool,
        page_path: &str,
    ) -> String {
        let mut out = String::new();
        let children: Vec<&&Comment> = all_comments
            .iter()
            .filter(|c| c.parent_id == parent_id)
            .collect();

        // Sort children by score (descending), then chronologically as tiebreaker
        let mut children = children;
        children.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.timestamp.cmp(&b.timestamp))
        });

        for (i, comment) in children.iter().enumerate() {
            let time_ago = format_time_ago(comment.timestamp);
            let c_name = sanitize_user_content(&comment.name);
            let c_body = sanitize_user_content(&comment.body);

            // Count replies to this comment
            let n_replies = all_comments
                .iter()
                .filter(|c| c.parent_id == comment.id)
                .count();
            let replies_label = match n_replies {
                0 => String::new(),
                1 => " · 1 reply".to_string(),
                n => format!(" · {n} replies"),
            };

            let points_label = if comment.score == 1 {
                "1 point".to_string()
            } else {
                format!("{} points", comment.score)
            };

            let reply_link = if logged_in {
                format!(
                    " [text dim]·[/text] [link href=\"{page_path}?item={link_id}&reply={cid}\"][text fg=white dim]reply[/text][/link]",
                    page_path = page_path,
                    link_id = link_id,
                    cid = comment.id,
                )
            } else {
                String::new()
            };

            // First child [text] with inline children becomes the summary line,
            // rendered on the ▶/▼ row with links (upvote, reply).
            let vote_markup = if logged_in {
                format!(
                    "[button action=submit target=\"{page_path}?item={link_id}&cvote={}\"]▲[/button]",
                    comment.id,
                )
            } else {
                String::new()
            };
            out.push_str(&format!(
                "[details open]\n\
                 [text][text bold]{c_name}[/text] [text dim]· {points_label} · {time_ago}{replies_label} ·[/text] \
                 {vote_markup}{reply_link}[/text]\n\
                 [text]{body}[/text]\n",
                c_name = c_name,
                points_label = points_label,
                time_ago = time_ago,
                replies_label = replies_label,
                vote_markup = vote_markup,
                reply_link = reply_link,
                body = c_body,
            ));

            // Insert reply form if this comment is the reply target and user is logged in
            if logged_in && reply_to == Some(comment.id) {
                out.push_str(&self.render_comment_form(link_id, comment.id, page_path));
            }

            // Recurse into children (cap depth to prevent runaway nesting)
            if depth < 10 {
                let nested = self.render_comment_tree(
                    all_comments,
                    link_id,
                    comment.id,
                    depth + 1,
                    reply_to,
                    logged_in,
                    page_path,
                );
                if !nested.is_empty() {
                    out.push_str("[spacer lines=1 /]\n");
                    out.push_str(&nested);
                }
            }

            out.push_str("[/details]\n");
            if i + 1 < children.len() {
                out.push_str("[spacer lines=1 /]\n");
            }
        }
        out
    }
}

/// Link aggregator plugin. Provides `{{links}}` marker.
pub(crate) struct HnPlugin {
    stores: HashMap<PathBuf, LinkStore>,
}

impl HnPlugin {
    pub(crate) fn new() -> Self {
        HnPlugin {
            stores: HashMap::new(),
        }
    }

    fn get_store(&mut self, aml_path: &Path) -> &mut LinkStore {
        self.stores
            .entry(aml_path.to_path_buf())
            .or_insert_with(|| {
                let links_path = aml_path.with_extension("links.tsv");
                let comments_path = aml_path.with_extension("links.comments.tsv");
                LinkStore::load(links_path, comments_path)
            })
    }
}

impl PagePlugin for HnPlugin {
    fn marker(&self) -> &str {
        "{{links}}"
    }

    fn input_key(&self) -> Option<&str> {
        Some("links")
    }

    fn render(
        &mut self,
        aml_path: &Path,
        query: Option<&str>,
        _peer: SocketAddr,
        _param: Option<&str>,
        site_root: &Path,
        identity: Option<&str>,
    ) -> String {
        let params = parse_form_data(query.unwrap_or(""));
        let store = self.get_store(aml_path);
        let logged_in = identity.is_some();

        // Derive the URL path for this page from the filesystem path.
        // e.g. site_root=/srv/site, aml_path=/srv/site/index.aml → page_path="/index"
        // Canonicalize both paths to handle relative vs absolute mismatches.
        let canon_root = site_root
            .canonicalize()
            .unwrap_or_else(|_| site_root.to_path_buf());
        let canon_aml = aml_path
            .canonicalize()
            .unwrap_or_else(|_| aml_path.to_path_buf());
        let page_path = canon_aml
            .strip_prefix(&canon_root)
            .unwrap_or(canon_aml.as_ref())
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let page_path = format!("/{page_path}");

        // Determine view
        if let Some(item_str) = params.get("item")
            && let Ok(id) = item_str.parse::<u64>()
        {
            let reply_to = params.get("reply").and_then(|r| r.parse::<u64>().ok());
            return store.render_item_page(id, reply_to, logged_in, &page_path);
        }

        // Default: front page
        let mut out = store.render_front_page(&page_path, logged_in);

        // Submit form only shown to logged-in users
        if logged_in {
            out.push_str(&format!(
                "[hr style=single fg=white /]\n\
                 [spacer lines=1 /]\n\
                 [form action=\"{page_path}\"]\n\
                 [input name=\"title\" placeholder=\"title\" maxlen=200 /]\n\
                 [input name=\"url\" placeholder=\"url\" maxlen=500 /]\n\
                 [input name=\"text\" placeholder=\"text (optional)\" maxlen=500 /]\n\
                 [button action=submit]submit[/button]\n\
                 [/form]\n",
            ));
        } else {
            out.push_str(
                "[hr style=single fg=white /]\n\
                 [spacer lines=1 /]\n\
                 [link href=\"/login\"][text fg=white]log in to submit[/text][/link]\n",
            );
        }

        out
    }

    fn handle_input(
        &mut self,
        aml_path: &Path,
        fields: &FormData,
        query: Option<&str>,
        identity: Option<&str>,
    ) -> Result<bool, String> {
        // Require authentication for all submissions
        let name = identity.ok_or("You must be logged in to post.")?;

        let params = parse_form_data(query.unwrap_or(""));
        let store = self.get_store(aml_path);

        // Votes are authenticated INPUT actions and are deduplicated by user.
        if let Some(vote_str) = params.get("vote") {
            let id = vote_str
                .parse::<u64>()
                .map_err(|_| "Invalid link ID.".to_string())?;
            return store
                .upvote(id, name)?
                .then_some(true)
                .ok_or_else(|| "You already voted on this story.".to_string());
        }
        if let Some(vote_str) = params.get("cvote") {
            let id = vote_str
                .parse::<u64>()
                .map_err(|_| "Invalid comment ID.".to_string())?;
            return store
                .upvote_comment(id, name)?
                .then_some(true)
                .ok_or_else(|| "You already voted on this comment.".to_string());
        }

        // If query has "item", this is a comment submission
        if let Some(item_str) = params.get("item") {
            let link_id = item_str
                .parse::<u64>()
                .map_err(|_| "Invalid link ID.".to_string())?;
            let parent_id = params
                .get("parent")
                .and_then(|p| p.parse::<u64>().ok())
                .unwrap_or(0);

            let body = sanitize_field(
                fields.get("msg").map(|s| s.as_str()).unwrap_or(""),
                MAX_BODY_LEN,
            );

            if body.is_empty() {
                return Err("Comment is required.".into());
            }
            if !store.links.iter().any(|l| l.id == link_id) {
                return Err("Link not found.".into());
            }
            if parent_id != 0
                && !store
                    .comments
                    .iter()
                    .any(|comment| comment.id == parent_id && comment.link_id == link_id)
            {
                return Err("Reply target not found for this story.".into());
            }

            store.add_comment(link_id, parent_id, name.to_string(), body)?;
            return Ok(true);
        }

        // Otherwise: link submission
        let title = sanitize_field(
            fields.get("title").map(|s| s.as_str()).unwrap_or(""),
            MAX_TITLE_LEN,
        );
        let raw_url = fields.get("url").map(|s| s.as_str()).unwrap_or("");
        let mut url = sanitize_field(raw_url, MAX_URL_LEN);

        if title.is_empty() {
            return Err("Title is required.".into());
        }
        let text = sanitize_field(
            fields.get("text").map(|s| s.as_str()).unwrap_or(""),
            MAX_BODY_LEN,
        );
        if url.is_empty() && text.is_empty() {
            return Err("A URL or text body is required.".into());
        }

        // Auto-prepend https:// if no scheme
        if !url.is_empty()
            && !url.starts_with("http://")
            && !url.starts_with("https://")
            && !url.starts_with("atp://")
        {
            url = format!("https://{url}");
        }
        if url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '"' | '[' | ']'))
        {
            return Err("URL contains invalid characters.".into());
        }

        let link_id = store.add_link(name.to_string(), title, url)?;

        // If a description/text was provided, post it as the first comment
        if !text.is_empty()
            && let Err(error) = store.add_comment(link_id, 0, name.to_string(), text)
        {
            // The story is already durable; report the partial failure rather
            // than claiming its accompanying text was saved.
            return Err(format!(
                "Story saved, but its text could not be saved: {error}"
            ));
        }

        Ok(true)
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| format!("Could not write content: {e}"))?;
    file.write_all(content.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Could not write content: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Could not commit content: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path.clone(), comments_path.clone());
        assert!(store.links.is_empty());
        assert!(store.comments.is_empty());

        store
            .add_link(
                "alice".into(),
                "Cool project".into(),
                "https://example.com".into(),
            )
            .unwrap();
        store
            .add_link(
                "bob".into(),
                "Neat article".into(),
                "https://blog.example.com/neat".into(),
            )
            .unwrap();
        assert_eq!(store.links.len(), 2);

        // Reload from disk
        let store2 = LinkStore::load(links_path, comments_path);
        assert_eq!(store2.links.len(), 2);
        assert_eq!(store2.links[0].name, "alice");
        assert_eq!(store2.links[0].title, "Cool project");
        assert_eq!(store2.links[1].name, "bob");
    }

    #[test]
    fn link_store_comments_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path.clone(), comments_path.clone());
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;

        store
            .add_comment(link_id, 0, "bob".into(), "Great post!".into())
            .unwrap();
        let comment_id = store.comments[0].id;
        store
            .add_comment(link_id, comment_id, "alice".into(), "Thanks!".into())
            .unwrap();

        assert_eq!(store.comments.len(), 2);
        assert_eq!(store.comments[0].parent_id, 0);
        assert_eq!(store.comments[1].parent_id, comment_id);

        // Reload
        let store2 = LinkStore::load(links_path, comments_path);
        assert_eq!(store2.comments.len(), 2);
        assert_eq!(store2.comments[0].name, "bob");
        assert_eq!(store2.comments[1].name, "alice");
        assert_eq!(store2.comments[1].parent_id, comment_id);
    }

    #[test]
    fn link_store_upvote_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;
        assert_eq!(store.links[0].score, 1); // initial score
        assert!(store.upvote(link_id, "alice").unwrap()); // first vote succeeds
        assert_eq!(store.links[0].score, 2);
        assert!(!store.upvote(link_id, "alice").unwrap()); // duplicate vote rejected
        assert_eq!(store.links[0].score, 2);

        // A different authenticated user can vote.
        assert!(store.upvote(link_id, "bob").unwrap());
        assert_eq!(store.links[0].score, 3);
    }

    #[test]
    fn votes_remain_deduplicated_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path.clone(), comments_path.clone());
        let link_id = store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        assert!(store.upvote(link_id, "alice").unwrap());
        drop(store);

        let mut reloaded = LinkStore::load(links_path, comments_path);
        assert!(!reloaded.upvote(link_id, "Alice").unwrap());
        assert!(reloaded.upvote(link_id, "bob").unwrap());
    }

    #[test]
    fn link_store_front_page_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);

        // Empty state
        let rendered = store.render_front_page("/test", false);
        assert!(rendered.contains("No links yet"));

        // Add some links
        store
            .add_link(
                "alice".into(),
                "Cool Site".into(),
                "https://cool.example.com/page".into(),
            )
            .unwrap();
        store
            .add_link(
                "bob".into(),
                "Neat Article".into(),
                "https://blog.example.com/neat".into(),
            )
            .unwrap();

        let rendered = store.render_front_page("/test", true);
        assert!(rendered.contains("Cool Site"), "should contain link title");
        assert!(
            rendered.contains("[link href=\"https://cool.example.com/page\"]Cool Site[/link]"),
            "story title should link to its submitted URL: {rendered}"
        );
        assert!(
            rendered.contains("action=submit target=\"/test?vote="),
            "authenticated votes should use INPUT buttons: {rendered}"
        );
        assert!(!rendered.contains("href=\"/test?vote="));
        assert!(
            rendered.contains("cool.example.com"),
            "should contain domain"
        );
        assert!(rendered.contains("alice"), "should contain submitter name");
        assert!(rendered.contains("\u{25B2}"), "should have upvote link (▲)");
        assert!(
            rendered.contains("discuss"),
            "should have discuss link for 0 comments"
        );
    }

    #[test]
    fn link_store_item_page_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link(
                "alice".into(),
                "Test Link".into(),
                "https://example.com".into(),
            )
            .unwrap();
        let link_id = store.links[0].id;

        // Item page with no comments
        let rendered = store.render_item_page(link_id, None, true, "/test");
        assert!(rendered.contains("Test Link"), "should show title");
        assert!(
            rendered.contains("No comments yet"),
            "should show empty state"
        );
        assert!(
            rendered.contains("Add a Comment"),
            "should have comment form"
        );

        // Add a comment
        store
            .add_comment(link_id, 0, "bob".into(), "Great link!".into())
            .unwrap();
        let rendered = store.render_item_page(link_id, None, true, "/test");
        assert!(rendered.contains("bob"), "should show commenter name");
        assert!(rendered.contains("Great link!"), "should show comment body");
        assert!(rendered.contains("reply"), "should have reply link");
    }

    #[test]
    fn link_store_threaded_comments() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;

        store
            .add_comment(link_id, 0, "bob".into(), "Top level comment".into())
            .unwrap();
        let top_comment_id = store.comments[0].id;

        store
            .add_comment(
                link_id,
                top_comment_id,
                "charlie".into(),
                "Reply to bob".into(),
            )
            .unwrap();

        let rendered = store.render_item_page(link_id, None, true, "/test");
        assert!(rendered.contains("Top level comment"));
        assert!(rendered.contains("Reply to bob"));
        // Charlie's comment should be nested inside bob's [details]
        assert!(
            rendered.contains("charlie"),
            "should show nested commenter name"
        );
    }

    #[test]
    fn link_store_reply_form_injection() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;

        store
            .add_comment(link_id, 0, "bob".into(), "Comment".into())
            .unwrap();
        let comment_id = store.comments[0].id;

        // Request reply form for this comment
        let rendered = store.render_item_page(link_id, Some(comment_id), true, "/test");
        // Should have a reply form with the parent ID in the action
        assert!(
            rendered.contains(&format!("parent={comment_id}")),
            "reply form should encode parent ID in action URL"
        );
    }

    #[test]
    fn link_submission_auto_prepends_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("links.aml");
        std::fs::write(&aml, "[page mode=document]\n{{links}}\n[/page]").unwrap();

        let mut plugin = HnPlugin::new();
        let mut fields = FormData::default();
        fields.insert("title".into(), "Cool Site".into());
        fields.insert("url".into(), "example.com/cool".into());

        assert!(
            plugin
                .handle_input(&aml, &fields, None, Some("alice"))
                .unwrap()
        );
        let store = plugin.get_store(&aml);
        assert_eq!(store.links.len(), 1);
        assert_eq!(store.links[0].url, "https://example.com/cool");
    }

    #[test]
    fn link_submission_with_description() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("links.aml");
        std::fs::write(&aml, "[page mode=document]\n{{links}}\n[/page]").unwrap();

        let mut plugin = HnPlugin::new();
        let mut fields = FormData::default();
        fields.insert("title".into(), "Cool Site".into());
        fields.insert("url".into(), "https://example.com".into());
        fields.insert("text".into(), "Check this out, it's really neat!".into());

        // identity is the resolved username, not a raw token
        assert!(
            plugin
                .handle_input(&aml, &fields, None, Some("alice"))
                .unwrap()
        );
        let store = plugin.get_store(&aml);
        assert_eq!(store.links.len(), 1);
        assert_eq!(
            store.comments.len(),
            1,
            "description should become first comment"
        );
        assert_eq!(store.comments[0].link_id, store.links[0].id);
        assert_eq!(store.comments[0].parent_id, 0);
        assert_eq!(store.comments[0].name, "alice");
        assert!(store.comments[0].body.contains("really neat"));
    }

    #[test]
    fn text_only_submission_is_supported() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("links.aml");
        std::fs::write(&aml, "[page mode=document]\n{{links}}\n[/page]").unwrap();

        let mut plugin = HnPlugin::new();
        let fields = FormData {
            fields: vec![
                ("title".into(), "Ask DN: Test".into()),
                ("url".into(), String::new()),
                ("text".into(), "A text-only discussion".into()),
            ],
        };

        assert!(
            plugin
                .handle_input(&aml, &fields, None, Some("alice"))
                .unwrap()
        );
        let store = plugin.get_store(&aml);
        assert_eq!(store.links[0].url, "");
        assert_eq!(store.comments[0].body, "A text-only discussion");
        let rendered = store.render_front_page("/links", true);
        assert!(rendered.contains("Ask DN: Test"));
        assert!(!rendered.contains("href=\"\""));
        assert!(!rendered.contains("()"));
    }

    #[test]
    fn get_queries_cannot_cast_votes() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("links.aml");
        std::fs::write(&aml, "[page mode=document]\n{{links}}\n[/page]").unwrap();
        let mut plugin = HnPlugin::new();
        let fields = FormData {
            fields: vec![
                ("title".into(), "Post".into()),
                ("url".into(), "https://example.com".into()),
            ],
        };
        plugin
            .handle_input(&aml, &fields, None, Some("alice"))
            .unwrap();

        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        plugin.render(&aml, Some("vote=1"), peer, None, dir.path(), Some("alice"));
        assert_eq!(plugin.get_store(&aml).links[0].score, 1);
    }

    #[test]
    fn replies_must_target_a_comment_on_the_same_story() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("links.aml");
        std::fs::write(&aml, "[page mode=document]\n{{links}}\n[/page]").unwrap();
        let mut plugin = HnPlugin::new();
        let store = plugin.get_store(&aml);
        let first = store
            .add_link("alice".into(), "One".into(), "https://one.test".into())
            .unwrap();
        let second = store
            .add_link("alice".into(), "Two".into(), "https://two.test".into())
            .unwrap();
        let parent = store
            .add_comment(first, 0, "alice".into(), "Parent".into())
            .unwrap();
        let fields = FormData {
            fields: vec![("msg".into(), "Cross-story reply".into())],
        };

        let result = plugin.handle_input(
            &aml,
            &fields,
            Some(&format!("item={second}&parent={parent}")),
            Some("bob"),
        );
        assert_eq!(
            result.unwrap_err(),
            "Reply target not found for this story."
        );
    }

    #[test]
    fn link_store_escapes_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link(
                "[evil]".into(),
                "[text fg=red]injected[/text]".into(),
                "https://example.com".into(),
            )
            .unwrap();

        let rendered = store.render_front_page("/test", false);
        assert!(rendered.contains("[[evil]]"), "name should be escaped");
        assert!(
            rendered.contains("[[text fg=red]]injected[[/text]]"),
            "title should be escaped"
        );
    }

    #[test]
    fn comment_upvote_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;
        store
            .add_comment(link_id, 0, "bob".into(), "Great post!".into())
            .unwrap();
        let comment_id = store.comments[0].id;
        assert_eq!(store.comments[0].score, 1);
        assert!(store.upvote_comment(comment_id, "alice").unwrap());
        assert_eq!(store.comments[0].score, 2);
        assert!(!store.upvote_comment(comment_id, "alice").unwrap()); // dedup
        assert_eq!(store.comments[0].score, 2);

        assert!(store.upvote_comment(comment_id, "bob").unwrap());
        assert_eq!(store.comments[0].score, 3);
    }

    #[test]
    fn comment_tsv_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        // Write old-format comment TSV (6 fields, no score)
        std::fs::write(&comments_path, "1\t1\t0\t1000000\tbob\tHello").unwrap();

        let store = LinkStore::load(links_path, comments_path);
        assert_eq!(store.comments.len(), 1);
        assert_eq!(store.comments[0].score, 1); // defaults to 1
    }

    #[test]
    fn comments_sorted_by_score() {
        let dir = tempfile::tempdir().unwrap();
        let links_path = dir.path().join("test.links.tsv");
        let comments_path = dir.path().join("test.links.comments.tsv");

        let mut store = LinkStore::load(links_path, comments_path);
        store
            .add_link("alice".into(), "Post".into(), "https://x.com".into())
            .unwrap();
        let link_id = store.links[0].id;

        store
            .add_comment(link_id, 0, "first".into(), "First comment".into())
            .unwrap();
        store
            .add_comment(link_id, 0, "second".into(), "Second comment".into())
            .unwrap();
        let second_id = store.comments[1].id;

        // Upvote the second comment
        store.upvote_comment(second_id, "alice").unwrap();

        let rendered = store.render_item_page(link_id, None, false, "/test");
        let pos_second = rendered.find("Second comment").unwrap();
        let pos_first = rendered.find("First comment").unwrap();
        assert!(
            pos_second < pos_first,
            "Higher-scored comment should appear first"
        );
    }
}
