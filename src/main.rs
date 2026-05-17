use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const BIN_NAME: &str = "gentoo-overlay-new-packages-to-rss";
const RECORD_SEPARATOR: char = '\x1e';
const UNIT_SEPARATOR: char = '\x1f';

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
enum Error {
    Args(String),
    Io(std::io::Error),
    Git { args: Vec<String>, stderr: String },
    Utf8(std::string::FromUtf8Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Args(message) => write!(f, "{message}"),
            Error::Io(err) => write!(f, "{err}"),
            Error::Git { args, stderr } => {
                write!(f, "git {} failed: {}", args.join(" "), stderr.trim())
            }
            Error::Utf8(err) => write!(f, "{err}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Error::Utf8(err)
    }
}

#[derive(Debug, Clone)]
struct Config {
    repo: PathBuf,
    output: Option<PathBuf>,
    repo_url: Option<String>,
    self_url: Option<String>,
    title: Option<String>,
    description: Option<String>,
    max_items: Option<usize>,
    include_root: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PackageItem {
    package: String,
    commit: String,
    commit_subject: String,
    author: String,
    date_rfc2822: String,
    description: String,
    homepage: String,
    license: String,
    ebuild_path: String,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(output) => {
            eprintln!("Wrote {}", output.display());
            ExitCode::SUCCESS
        }
        Err(Error::Args(message)) if message == "help" => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<PathBuf> {
    let config = Config::from_args(env::args().skip(1))?;
    generate(config)
}

impl Config {
    fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut config = Config {
            repo: PathBuf::from("."),
            output: None,
            repo_url: None,
            self_url: None,
            title: None,
            description: None,
            max_items: None,
            include_root: false,
        };

        let mut args = args.into_iter().map(Into::into).peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    return Err(Error::Args("help".to_string()));
                }
                "--repo" => config.repo = PathBuf::from(take_value(&mut args, "--repo")?),
                "--output" => {
                    config.output = Some(PathBuf::from(take_value(&mut args, "--output")?))
                }
                "--repo-url" => config.repo_url = Some(take_value(&mut args, "--repo-url")?),
                "--self-url" => config.self_url = Some(take_value(&mut args, "--self-url")?),
                "--title" => config.title = Some(take_value(&mut args, "--title")?),
                "--description" => {
                    config.description = Some(take_value(&mut args, "--description")?)
                }
                "--max-items" => {
                    let value = take_value(&mut args, "--max-items")?;
                    config.max_items = Some(value.parse::<usize>().map_err(|_| {
                        Error::Args(format!(
                            "--max-items expects a positive integer, got {value:?}"
                        ))
                    })?);
                }
                "--include-root" => config.include_root = true,
                unknown => return Err(Error::Args(format!("unknown argument: {unknown}"))),
            }
        }

        Ok(config)
    }
}

fn take_value<I>(args: &mut std::iter::Peekable<I>, name: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| Error::Args(format!("{name} expects a value")))
}

fn print_help() {
    println!(
        "{BIN_NAME}

Generate an RSS feed of newly added packages in a Gentoo overlay git repository.

Usage:
  {BIN_NAME} [options]

Options:
  --repo PATH            Git repository to inspect (default: current directory)
  --output PATH          RSS path to write (default: public/<repo-name>.rss)
  --repo-url URL         Public repository URL for item links (default: origin remote)
  --self-url URL         Public URL of the generated RSS feed
  --title TEXT           RSS channel title
  --description TEXT     RSS channel description
  --max-items N          Keep only the newest N items
  --include-root         Include package metadata added by the root commit
  -h, --help             Show this help
"
    );
}

fn generate(config: Config) -> Result<PathBuf> {
    let repo = repo_root(&config.repo)?;
    let repo_name = repo_name(&repo)?;
    let repo_url = config
        .repo_url
        .as_deref()
        .map(normalize_repo_url)
        .or_else(|| discover_repo_url(&repo).ok())
        .unwrap_or_else(|| repo.display().to_string());
    let output = config
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("public").join(format!("{repo_name}.rss")));
    let output = if output.is_absolute() {
        output
    } else {
        repo.join(output)
    };

    let mut items = new_package_items(&repo, &config)?;
    items.reverse();
    if let Some(max_items) = config.max_items {
        items.truncate(max_items);
    }

    let channel_title = config
        .title
        .unwrap_or_else(|| format!("{repo_name}: new Gentoo packages"));
    let channel_description = config
        .description
        .unwrap_or_else(|| format!("Newly added packages in the {repo_name} Gentoo overlay"));
    let rss = render_rss(
        &channel_title,
        &repo_url,
        config.self_url.as_deref(),
        &channel_description,
        &items,
    );

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, rss)?;

    Ok(output)
}

fn repo_root(repo: &Path) -> Result<PathBuf> {
    let root = git_output(repo, ["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root.trim()))
}

fn repo_name(repo: &Path) -> Result<String> {
    let gentoo_name = repo.join("profiles").join("repo_name");
    if gentoo_name.exists() {
        let name = fs::read_to_string(gentoo_name)?;
        let name = name.trim();
        if !name.is_empty() {
            return Ok(name.to_string());
        }
    }

    Ok(repo
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("overlay")
        .to_string())
}

fn discover_repo_url(repo: &Path) -> Result<String> {
    let remote = git_output(repo, ["config", "--get", "remote.origin.url"])?;
    Ok(normalize_repo_url(remote.trim()))
}

fn new_package_items(repo: &Path, config: &Config) -> Result<Vec<PackageItem>> {
    let format = format!(
        "--format=format:{RECORD_SEPARATOR}%H{UNIT_SEPARATOR}%P{UNIT_SEPARATOR}%aD{UNIT_SEPARATOR}%ae{UNIT_SEPARATOR}%an{UNIT_SEPARATOR}%s"
    );
    let log = git_output(
        repo,
        [
            "log",
            "--reverse",
            "--name-status",
            "--diff-filter=A",
            &format,
            "--",
            "*/*/metadata.xml",
        ],
    )?;
    let mut items = Vec::new();
    let mut seen_in_commit = HashSet::new();

    for record in log
        .split(RECORD_SEPARATOR)
        .filter(|record| !record.trim().is_empty())
    {
        let mut lines = record.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut header_fields = header.split(UNIT_SEPARATOR);
        let commit = header_fields.next().unwrap_or_default();
        let parents = header_fields.next().unwrap_or_default();
        let date_rfc2822 = header_fields.next().unwrap_or_default();
        let author_email = header_fields.next().unwrap_or_default();
        let author_name = header_fields.next().unwrap_or_default();
        let subject = header_fields.next().unwrap_or_default();
        let author = rss_author(author_email, author_name).unwrap_or_default();

        let has_parent = parents.split_whitespace().next().is_some();
        if !has_parent && !config.include_root {
            continue;
        }

        seen_in_commit.clear();
        for line in lines {
            let Some(path) = added_path_from_name_status(line) else {
                continue;
            };
            let Some(package) = package_from_metadata_path(path) else {
                continue;
            };
            if !seen_in_commit.insert(package.to_string()) {
                continue;
            }
            let Some(ebuild_path) = select_ebuild(repo, commit, package)? else {
                continue;
            };
            let ebuild = git_output(repo, ["show", &format!("{commit}:{ebuild_path}")])?;
            let vars = EbuildVars::from_ebuild(&ebuild);

            items.push(PackageItem {
                package: package.to_string(),
                commit: commit.to_string(),
                commit_subject: subject.to_string(),
                author: author.clone(),
                date_rfc2822: date_rfc2822.to_string(),
                description: vars.description.unwrap_or_default(),
                homepage: vars.homepage.unwrap_or_default(),
                license: vars.license.unwrap_or_default(),
                ebuild_path,
            });
        }
    }

    Ok(items)
}

fn added_path_from_name_status(line: &str) -> Option<&str> {
    let (status, path) = line.split_once('\t')?;
    (status == "A").then_some(path)
}

fn package_from_metadata_path(path: &str) -> Option<&str> {
    let mut parts = path.split('/');
    let category = parts.next()?;
    let package = parts.next()?;
    let filename = parts.next()?;
    if parts.next().is_none()
        && !category.is_empty()
        && !package.is_empty()
        && filename == "metadata.xml"
    {
        Some(path.strip_suffix("/metadata.xml")?)
    } else {
        None
    }
}

fn select_ebuild(repo: &Path, commit: &str, package: &str) -> Result<Option<String>> {
    let tree = git_output(
        repo,
        ["ls-tree", "-r", "--name-only", commit, "--", package],
    )?;
    let mut ebuilds: Vec<_> = tree
        .lines()
        .filter(|path| path.ends_with(".ebuild"))
        .map(str::to_string)
        .collect();
    ebuilds.sort();
    Ok(ebuilds
        .iter()
        .rev()
        .find(|path| !path.ends_with("-9999.ebuild"))
        .or_else(|| ebuilds.last())
        .cloned())
}

#[derive(Debug, Default, Eq, PartialEq)]
struct EbuildVars {
    description: Option<String>,
    homepage: Option<String>,
    license: Option<String>,
}

impl EbuildVars {
    fn from_ebuild(input: &str) -> Self {
        EbuildVars {
            description: assignment_value(input, "DESCRIPTION"),
            homepage: assignment_value(input, "HOMEPAGE").and_then(|value| {
                value
                    .split_whitespace()
                    .find(|part| is_valid_http_url(part))
                    .map(str::to_string)
            }),
            license: assignment_value(input, "LICENSE"),
        }
    }
}

fn is_valid_http_url(value: &str) -> bool {
    (value.starts_with("http://") || value.starts_with("https://"))
        && value.chars().all(|ch| {
            ch.is_ascii()
                && !ch.is_ascii_control()
                && !ch.is_ascii_whitespace()
                && !matches!(
                    ch,
                    '<' | '>' | '"' | '\'' | '{' | '}' | '|' | '\\' | '^' | '`'
                )
        })
}

fn rss_author(email: &str, name: &str) -> Option<String> {
    is_valid_email(email).then(|| {
        if name.trim().is_empty() {
            email.to_string()
        } else {
            format!("{email} ({})", name.trim())
        }
    })
}

fn is_valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && value.chars().all(|ch| {
            ch.is_ascii()
                && !ch.is_ascii_control()
                && !ch.is_ascii_whitespace()
                && !matches!(ch, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']')
        })
}

fn assignment_value(input: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let mut lines = input.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || !trimmed.starts_with(&prefix) {
            continue;
        }
        let value = trimmed[prefix.len()..].trim_start();
        return Some(parse_shell_value(value, &mut lines));
    }
    None
}

fn parse_shell_value<'a, I>(value: &str, lines: &mut I) -> String
where
    I: Iterator<Item = &'a str>,
{
    let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return collapse_space(value.split('#').next().unwrap_or_default());
    };

    let mut output = String::new();
    let mut escaped = false;
    for ch in value[quote.len_utf8()..].chars() {
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' && quote == '"' {
            escaped = true;
        } else if ch == quote {
            return collapse_space(&output);
        } else {
            output.push(ch);
        }
    }

    for line in lines {
        output.push('\n');
        let mut escaped = false;
        for ch in line.chars() {
            if escaped {
                output.push(ch);
                escaped = false;
            } else if ch == '\\' && quote == '"' {
                escaped = true;
            } else if ch == quote {
                return collapse_space(&output);
            } else {
                output.push(ch);
            }
        }
    }

    collapse_space(&output)
}

fn collapse_space(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_rss(
    title: &str,
    repo_url: &str,
    self_url: Option<&str>,
    description: &str,
    items: &[PackageItem],
) -> String {
    let build_date = items
        .first()
        .map(|item| item.date_rfc2822.clone())
        .unwrap_or_else(current_rfc2822_utc);
    let mut rss = String::new();
    rss.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    rss.push_str("<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n");
    rss.push_str("  <channel>\n");
    push_tag(&mut rss, 4, "title", title);
    push_tag(&mut rss, 4, "link", repo_url);
    push_tag(&mut rss, 4, "description", description);
    if let Some(self_url) = self_url.filter(|url| is_valid_http_url(url)) {
        push_atom_self_link(&mut rss, self_url);
    }
    push_tag(&mut rss, 4, "lastBuildDate", &build_date);
    push_tag(&mut rss, 4, "generator", BIN_NAME);
    push_tag(&mut rss, 4, "ttl", "1440");

    for item in items {
        let package_url = repo_link(repo_url, &["tree", &item.commit, &item.package]);
        let commit_url = repo_link(repo_url, &["commit", &item.commit]);
        let link = if item.homepage.is_empty() {
            package_url.as_str()
        } else {
            item.homepage.as_str()
        };
        let html_description = item_description(item, &package_url, &commit_url);

        rss.push_str("    <item>\n");
        push_tag(&mut rss, 6, "title", &item.package);
        push_tag(&mut rss, 6, "link", link);
        push_cdata_tag(&mut rss, 6, "description", &html_description);
        push_tag(&mut rss, 6, "pubDate", &item.date_rfc2822);
        if !item.author.is_empty() {
            push_tag(&mut rss, 6, "author", &item.author);
        }
        push_guid(&mut rss, &format!("{}:{}", item.commit, item.package));
        rss.push_str("    </item>\n");
    }

    rss.push_str("  </channel>\n");
    rss.push_str("</rss>\n");
    rss
}

fn item_description(item: &PackageItem, package_url: &str, commit_url: &str) -> String {
    let mut parts = Vec::new();
    if !item.description.is_empty() {
        parts.push(xml_escape(&item.description));
    }
    if !item.commit_subject.is_empty() {
        parts.push(format!(
            "Commit title: {}",
            xml_escape(&item.commit_subject)
        ));
    }
    parts.push(format!("Package: {}", xml_escape(&item.package)));
    parts.push(format!("Ebuild: {}", xml_escape(&item.ebuild_path)));
    if !item.homepage.is_empty() {
        parts.push(format!(
            "Homepage: <a href=\"{}\">{}</a>",
            xml_escape(&item.homepage),
            xml_escape(&item.homepage)
        ));
    }
    if !item.license.is_empty() {
        parts.push(format!("License: {}", xml_escape(&item.license)));
    }
    parts.push(format!(
        "<a href=\"{}\">Package directory</a>",
        xml_escape(package_url)
    ));
    parts.push(format!("<a href=\"{}\">Commit</a>", xml_escape(commit_url)));
    parts.join("<br/>\n")
}

fn push_tag(rss: &mut String, indent: usize, tag: &str, value: &str) {
    rss.push_str(&" ".repeat(indent));
    rss.push('<');
    rss.push_str(tag);
    rss.push('>');
    rss.push_str(&xml_escape(value));
    rss.push_str("</");
    rss.push_str(tag);
    rss.push_str(">\n");
}

fn push_cdata_tag(rss: &mut String, indent: usize, tag: &str, value: &str) {
    rss.push_str(&" ".repeat(indent));
    rss.push('<');
    rss.push_str(tag);
    rss.push_str("><![CDATA[");
    rss.push_str(&value.replace("]]>", "]]]]><![CDATA[>"));
    rss.push_str("]]></");
    rss.push_str(tag);
    rss.push_str(">\n");
}

fn push_guid(rss: &mut String, value: &str) {
    rss.push_str("      <guid isPermaLink=\"false\">");
    rss.push_str(&xml_escape(value));
    rss.push_str("</guid>\n");
}

fn push_atom_self_link(rss: &mut String, value: &str) {
    rss.push_str("    <atom:link href=\"");
    rss.push_str(&xml_escape(value));
    rss.push_str("\" rel=\"self\" type=\"application/rss+xml\"/>\n");
}

fn xml_escape(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(ch),
        }
    }
    output
}

fn repo_link(repo_url: &str, parts: &[&str]) -> String {
    if repo_url.starts_with("http://") || repo_url.starts_with("https://") {
        format!("{}/{}", repo_url.trim_end_matches('/'), parts.join("/"))
    } else {
        repo_url.to_string()
    }
}

fn normalize_repo_url(url: &str) -> String {
    let url = url.trim().trim_end_matches(".git");
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return format!("https://{host}/{}", path.trim_start_matches('/'));
    }
    if let Some(rest) = url.strip_prefix("ssh://git@")
        && let Some((host, path)) = rest.split_once('/')
    {
        return format!("https://{host}/{}", path.trim_start_matches('/'));
    }
    url.to_string()
}

fn current_rfc2822_utc() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_rfc2822_utc(now)
}

fn format_rfc2822_utc(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    let (year, month, day) = civil_from_days(days);
    let weekday =
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][(days + 4).rem_euclid(7) as usize];
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];

    format!("{weekday}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} +0000")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month as u32, day as u32)
}

fn git_output<I, S>(repo: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args_vec: Vec<_> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect();
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args_vec)
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?)
    } else {
        Err(Error::Git {
            args: args_vec,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ebuild_variables() {
        let vars = EbuildVars::from_ebuild(
            r#"
# DESCRIPTION="wrong"
DESCRIPTION="Fast <useful> package"
HOMEPAGE="
    https://example.com/project
    https://mirror.example/project
"
LICENSE='Apache-2.0 MIT'
"#,
        );

        assert_eq!(
            vars,
            EbuildVars {
                description: Some("Fast <useful> package".to_string()),
                homepage: Some("https://example.com/project".to_string()),
                license: Some("Apache-2.0 MIT".to_string()),
            }
        );
    }

    #[test]
    fn ignores_invalid_homepage_urls() {
        let vars = EbuildVars::from_ebuild(
            r#"
DESCRIPTION="Package"
HOMEPAGE="https://github.com/majn/${PN} https://example.com/project"
LICENSE="MIT"
"#,
        );

        assert_eq!(
            vars.homepage,
            Some("https://example.com/project".to_string())
        );
    }

    #[test]
    fn normalizes_github_ssh_urls() {
        assert_eq!(
            normalize_repo_url("git@github.com:microcai/gentoo-zh.git"),
            "https://github.com/microcai/gentoo-zh"
        );
        assert_eq!(
            normalize_repo_url("ssh://git@github.com/microcai/gentoo-zh.git"),
            "https://github.com/microcai/gentoo-zh"
        );
    }

    #[test]
    fn formats_rfc2822_utc() {
        assert_eq!(format_rfc2822_utc(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            format_rfc2822_utc(1_779_012_000),
            "Sun, 17 May 2026 10:00:00 +0000"
        );
    }

    #[test]
    fn generates_only_new_packages() {
        let repo = TestRepo::new("new-packages");
        repo.init();
        repo.write("profiles/repo_name", "test-overlay\n");
        repo.write(
            "app-root/rootpkg/metadata.xml",
            "<pkgmetadata></pkgmetadata>\n",
        );
        repo.write(
            "app-root/rootpkg/rootpkg-1.ebuild",
            r#"DESCRIPTION="Root package"
HOMEPAGE="https://root.example"
LICENSE="MIT"
"#,
        );
        repo.commit("initial import", "2024-01-01T00:00:00Z");

        repo.write(
            "app-root/rootpkg/rootpkg-2.ebuild",
            r#"DESCRIPTION="Root package update"
HOMEPAGE="https://root.example"
LICENSE="MIT"
"#,
        );
        repo.commit("app-root/rootpkg: add 2", "2024-01-02T00:00:00Z");

        repo.write(
            "app-existing/existing/existing-1.ebuild",
            r#"DESCRIPTION="Existing package"
HOMEPAGE="https://existing.example"
LICENSE="GPL-2"
"#,
        );
        repo.commit(
            "app-existing/existing: add ebuild first",
            "2024-01-03T00:00:00Z",
        );

        repo.write(
            "app-existing/existing/metadata.xml",
            "<pkgmetadata></pkgmetadata>\n",
        );
        repo.commit(
            "app-existing/existing: add metadata",
            "2024-01-04T00:00:00Z",
        );

        repo.write(
            "dev-util/newpkg/metadata.xml",
            "<pkgmetadata></pkgmetadata>\n",
        );
        repo.write(
            "dev-util/newpkg/newpkg-1.ebuild",
            r#"DESCRIPTION="Useful & fast"
HOMEPAGE="https://new.example/path?x=1&y=2"
LICENSE="Apache-2.0"
"#,
        );
        repo.commit("dev-util/newpkg: new package", "2024-01-05T00:00:00Z");

        let output = repo.path.join("public/feed.rss");
        let generated = generate(Config {
            repo: repo.path.clone(),
            output: Some(output.clone()),
            repo_url: Some("git@github.com:example/overlay.git".to_string()),
            self_url: Some("https://example.github.io/overlay/feed.rss".to_string()),
            title: None,
            description: None,
            max_items: None,
            include_root: false,
        })
        .expect("RSS generated");

        assert_eq!(generated, output);
        let rss = fs::read_to_string(output).expect("RSS can be read");
        assert!(rss.contains("<title>test-overlay: new Gentoo packages</title>"));
        assert!(rss.contains("dev-util/newpkg: new package"));
        assert!(rss.contains("Useful &amp; fast"));
        assert!(rss.contains("app-existing/existing: add metadata"));
        assert!(rss.contains("https://github.com/example/overlay/commit/"));
        assert!(!rss.contains("initial import"));
        assert!(!rss.contains("app-root/rootpkg: add 2"));
    }

    struct TestRepo {
        path: PathBuf,
    }

    impl TestRepo {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after unix epoch")
                .as_nanos();
            let path =
                env::temp_dir().join(format!("{BIN_NAME}-{name}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("test repo dir created");
            TestRepo { path }
        }

        fn init(&self) {
            self.git(["init", "-b", "master"], None);
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.path.join(relative);
            fs::create_dir_all(path.parent().expect("file has parent")).expect("dir created");
            fs::write(path, contents).expect("file written");
        }

        fn commit(&self, message: &str, date: &str) {
            self.git(["add", "."], None);
            self.git(["commit", "-m", message], Some(date));
        }

        fn git<I, S>(&self, args: I, date: Option<&str>)
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let mut command = Command::new("git");
            command.arg("-C").arg(&self.path);
            command
                .arg("-c")
                .arg("user.name=Test User")
                .arg("-c")
                .arg("user.email=test@example.com");
            command.args(args);
            if let Some(date) = date {
                command
                    .env("GIT_AUTHOR_DATE", date)
                    .env("GIT_COMMITTER_DATE", date);
            }
            let output = command.output().expect("git command can run");
            assert!(
                output.status.success(),
                "git failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
