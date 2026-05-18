use quick_xml::events::{BytesCData, BytesRef, BytesStart, BytesText, Event};
use quick_xml::{Reader, XmlVersion};
use std::collections::{HashMap, HashSet};
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
    list_patches: bool,
    github_author_links: bool,
    show_author_email: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PackageItem {
    package: String,
    commit: String,
    commit_subject: String,
    commit_body: String,
    author: String,
    author_email: String,
    author_name: String,
    author_github_username: String,
    date_rfc2822: String,
    description: String,
    metadata_description: String,
    use_flags: Vec<UseFlagDescription>,
    homepage: String,
    license: String,
    ebuild_path: String,
    patches: Vec<String>,
    distfiles: Vec<ManifestDistfile>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct UseFlagDescription {
    name: String,
    description: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ManifestDistfile {
    name: String,
    size_bytes: u64,
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
            list_patches: false,
            github_author_links: false,
            show_author_email: true,
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
                "--list-patches" => config.list_patches = true,
                "--github-author-links" => config.github_author_links = true,
                "--no-author-email" => config.show_author_email = false,
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
  --list-patches         Include package files/*.patch and files/*.diff names
  --github-author-links  Use GitHub API to link non-noreply author emails
  --no-author-email      Do not include author emails in item descriptions
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
    enrich_github_author_links(&mut items, config.github_author_links, &repo_url);

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
        config.show_author_email,
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
        "--format=format:{RECORD_SEPARATOR}%H{UNIT_SEPARATOR}%P{UNIT_SEPARATOR}%aD{UNIT_SEPARATOR}%ae{UNIT_SEPARATOR}%an{UNIT_SEPARATOR}%s{UNIT_SEPARATOR}%b"
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
        let mut header_fields = record.splitn(7, UNIT_SEPARATOR);
        let commit = header_fields.next().unwrap_or_default();
        let parents = header_fields.next().unwrap_or_default();
        let date_rfc2822 = header_fields.next().unwrap_or_default();
        let author_email = header_fields.next().unwrap_or_default();
        let author_name = header_fields.next().unwrap_or_default();
        let subject = header_fields.next().unwrap_or_default();
        let body_and_paths = header_fields.next().unwrap_or_default();
        let author = rss_author(author_email, author_name).unwrap_or_default();
        let author_github_username =
            github_username_from_noreply_email(author_email).unwrap_or_default();
        let (commit_body, added_paths) = commit_body_and_added_paths(body_and_paths);

        let has_parent = parents.split_whitespace().next().is_some();
        if !has_parent && !config.include_root {
            continue;
        }

        seen_in_commit.clear();
        for path in added_paths {
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
            let metadata = git_output(repo, ["show", &format!("{commit}:{path}")])?;
            let package_metadata = package_metadata(&metadata);
            let distfiles = package_manifest_distfiles(repo, commit, package)?;
            let patches = if config.list_patches {
                package_patch_names(repo, commit, package)?
            } else {
                Vec::new()
            };

            items.push(PackageItem {
                package: package.to_string(),
                commit: commit.to_string(),
                commit_subject: subject.to_string(),
                commit_body: commit_body.clone(),
                author: author.clone(),
                author_email: author_email.to_string(),
                author_name: author_name.trim().to_string(),
                author_github_username: author_github_username.clone(),
                date_rfc2822: date_rfc2822.to_string(),
                description: vars.description.unwrap_or_default(),
                metadata_description: package_metadata.description.unwrap_or_default(),
                use_flags: package_metadata.use_flags,
                homepage: vars.homepage.unwrap_or_default(),
                license: vars.license.unwrap_or_default(),
                ebuild_path,
                patches,
                distfiles,
            });
        }
    }

    Ok(items)
}

fn enrich_github_author_links(items: &mut [PackageItem], enabled: bool, repo_url: &str) {
    let mut author_resolver = GitHubAuthorResolver::new(enabled, repo_url);
    for item in items {
        if item.author_github_username.is_empty()
            && let Some(username) = author_resolver.username(&item.commit, &item.author_email)
        {
            item.author_github_username = username;
        }
    }
}

fn commit_body_and_added_paths(input: &str) -> (String, Vec<&str>) {
    let mut body_lines = Vec::new();
    let mut added_paths = Vec::new();

    for line in input.lines() {
        if let Some(path) = added_path_from_name_status(line)
            .filter(|path| package_from_metadata_path(path).is_some())
        {
            added_paths.push(path);
        } else {
            body_lines.push(line);
        }
    }

    (trim_body_lines(&body_lines), added_paths)
}

fn trim_body_lines(lines: &[&str]) -> String {
    let mut start = 0;
    let mut end = lines.len();

    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    lines[start..end]
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
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

fn package_manifest_distfiles(
    repo: &Path,
    commit: &str,
    package: &str,
) -> Result<Vec<ManifestDistfile>> {
    let manifest_path = format!("{package}/Manifest");
    let tree = git_output(
        repo,
        ["ls-tree", "--name-only", commit, "--", &manifest_path],
    )?;
    if !tree.lines().any(|path| path == manifest_path) {
        return Ok(Vec::new());
    }

    let manifest = git_output(repo, ["show", &format!("{commit}:{manifest_path}")])?;
    Ok(manifest_distfiles(&manifest))
}

fn manifest_distfiles(input: &str) -> Vec<ManifestDistfile> {
    input
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "DIST").then_some(())?;
            let name = fields.next()?;
            let size_bytes = fields.next()?.parse().ok()?;
            Some(ManifestDistfile {
                name: name.to_string(),
                size_bytes,
            })
        })
        .collect()
}

fn package_patch_names(repo: &Path, commit: &str, package: &str) -> Result<Vec<String>> {
    let files_dir = format!("{package}/files");
    let tree = git_output(
        repo,
        ["ls-tree", "-r", "--name-only", commit, "--", &files_dir],
    )?;
    let prefix = format!("{files_dir}/");
    let mut patches: Vec<_> = tree
        .lines()
        .filter(|path| is_patch_path(path))
        .filter_map(|path| path.strip_prefix(&prefix))
        .map(str::to_string)
        .collect();
    patches.sort();
    Ok(patches)
}

fn is_patch_path(path: &str) -> bool {
    let Some(name) = path.rsplit('/').next() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    [
        ".patch",
        ".diff",
        ".patch.gz",
        ".diff.gz",
        ".patch.bz2",
        ".diff.bz2",
        ".patch.xz",
        ".diff.xz",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
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

#[derive(Debug, Clone, Eq, PartialEq)]
struct GitHubRepo {
    owner: String,
    name: String,
}

struct GitHubAuthorResolver {
    enabled: bool,
    repo: Option<GitHubRepo>,
    token: Option<String>,
    cache: HashMap<String, String>,
}

impl GitHubAuthorResolver {
    fn new(enabled: bool, repo_url: &str) -> Self {
        GitHubAuthorResolver {
            enabled,
            repo: enabled.then(|| github_repo_from_url(repo_url)).flatten(),
            token: env::var("GITHUB_TOKEN")
                .or_else(|_| env::var("GH_TOKEN"))
                .ok()
                .filter(|token| !token.trim().is_empty()),
            cache: HashMap::new(),
        }
    }

    fn username(&mut self, commit: &str, email: &str) -> Option<String> {
        if let Some(username) = github_username_from_noreply_email(email) {
            return Some(username);
        }
        if !self.enabled || self.repo.is_none() {
            return None;
        }

        let cache_key = email.trim().to_ascii_lowercase();
        if !cache_key.is_empty()
            && let Some(username) = self.cache.get(&cache_key)
        {
            return Some(username.clone());
        }

        let username = self.fetch_commit_author_login(commit);
        if !cache_key.is_empty()
            && let Some(username) = &username
        {
            self.cache.insert(cache_key, username.clone());
        }
        username
    }

    fn fetch_commit_author_login(&self, commit: &str) -> Option<String> {
        let repo = self.repo.as_ref()?;
        let url = format!(
            "https://api.github.com/repos/{}/{}/commits/{}",
            repo.owner, repo.name, commit
        );
        let mut command = Command::new("curl");
        command
            .arg("-fsSL")
            .arg("-H")
            .arg("Accept: application/vnd.github+json")
            .arg("-H")
            .arg("X-GitHub-Api-Version: 2022-11-28");
        if let Some(token) = &self.token {
            command
                .arg("-H")
                .arg(format!("Authorization: Bearer {token}"));
        }
        let output = command.arg(url).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let json = String::from_utf8(output.stdout).ok()?;
        github_author_login_from_commit_json(&json)
    }
}

fn github_repo_from_url(url: &str) -> Option<GitHubRepo> {
    let normalized = normalize_repo_url(url);
    let path = normalized
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.to_string();
    let name = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(GitHubRepo { owner, name })
}

fn github_username_from_noreply_email(email: &str) -> Option<String> {
    let (local, domain) = email.trim().split_once('@')?;
    if !domain.eq_ignore_ascii_case("users.noreply.github.com") {
        return None;
    }

    let username = local
        .split_once('+')
        .map_or(local, |(_, username)| username);
    is_valid_github_username(username).then(|| username.to_string())
}

fn is_valid_github_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > 39 || username.starts_with('-') || username.ends_with('-')
    {
        return false;
    }

    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

fn github_author_login_from_commit_json(input: &str) -> Option<String> {
    let author = json_top_level_object_field(input, "author")?;
    let login = json_string_field(author, "login")?;
    is_valid_github_username(&login).then_some(login)
}

fn json_top_level_object_field<'a>(input: &'a str, field: &str) -> Option<&'a str> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        match byte {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'"' if depth == 1 => {
                let (key, next) = json_string_at(input, index)?;
                let colon = skip_json_whitespace(bytes, next);
                if bytes.get(colon) == Some(&b':') && key == field {
                    let value = skip_json_whitespace(bytes, colon + 1);
                    if bytes.get(value) == Some(&b'{') {
                        return json_object_at(input, value);
                    }
                }
                index = next;
                continue;
            }
            b'"' => in_string = true,
            _ => {}
        }
        index += 1;
    }

    None
}

fn json_string_field(input: &str, field: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        match byte {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'"' if depth == 1 => {
                let (key, next) = json_string_at(input, index)?;
                let colon = skip_json_whitespace(bytes, next);
                if bytes.get(colon) == Some(&b':') && key == field {
                    let value = skip_json_whitespace(bytes, colon + 1);
                    if bytes.get(value) == Some(&b'"') {
                        return json_string_at(input, value).map(|(value, _)| value);
                    }
                }
                index = next;
                continue;
            }
            b'"' => in_string = true,
            _ => {}
        }
        index += 1;
    }

    None
}

fn json_object_at(input: &str, start: usize) -> Option<&str> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = start;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return input.get(start..=index);
                }
            }
            b'"' => in_string = true,
            _ => {}
        }
        index += 1;
    }

    None
}

fn json_string_at(input: &str, start: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    (bytes.get(start) == Some(&b'"')).then_some(())?;
    let mut value = String::new();
    let mut escaped = false;
    let mut index = start + 1;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            value.push(byte as char);
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some((value, index + 1));
        } else {
            value.push(byte as char);
        }
        index += 1;
    }

    None
}

fn skip_json_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        index += 1;
    }
    index
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

#[derive(Debug, Eq, PartialEq)]
struct MetadataText {
    lang: Option<String>,
    text: String,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct PackageMetadata {
    description: Option<String>,
    use_flags: Vec<UseFlagDescription>,
}

fn package_metadata(input: &str) -> PackageMetadata {
    let mut reader = Reader::from_str(input);

    let mut descriptions = Vec::new();
    let mut longdescriptions = Vec::new();
    let mut use_flags = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if is_element(&start, b"pkgmetadata") => {
                read_metadata_children(
                    &mut reader,
                    &mut descriptions,
                    &mut longdescriptions,
                    &mut use_flags,
                );
                break;
            }
            Ok(Event::Empty(start)) if is_element(&start, b"pkgmetadata") => break,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    PackageMetadata {
        description: metadata_description_from_parts(descriptions, &longdescriptions),
        use_flags,
    }
}

#[cfg(test)]
fn metadata_description(input: &str) -> Option<String> {
    package_metadata(input).description
}

fn metadata_description_from_parts(
    descriptions: Vec<MetadataText>,
    longdescriptions: &[MetadataText],
) -> Option<String> {
    descriptions
        .into_iter()
        .map(|item| item.text)
        .find(|text| !text.is_empty())
        .or_else(|| {
            longdescriptions
                .iter()
                .find(|item| item.lang.as_deref() == Some("en"))
                .or_else(|| longdescriptions.first())
                .map(|item| item.text.clone())
                .filter(|text| !text.is_empty())
        })
}

fn read_metadata_children(
    reader: &mut Reader<&[u8]>,
    descriptions: &mut Vec<MetadataText>,
    longdescriptions: &mut Vec<MetadataText>,
    use_flags: &mut Vec<UseFlagDescription>,
) {
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if depth == 0 && is_element(&start, b"description") => {
                descriptions.push(read_metadata_text(reader, &start));
            }
            Ok(Event::Start(start)) if depth == 0 && is_element(&start, b"longdescription") => {
                longdescriptions.push(read_metadata_text(reader, &start));
            }
            Ok(Event::Start(start)) if depth == 0 && is_element(&start, b"use") => {
                read_use_flags(reader, use_flags);
            }
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(end)) => {
                if depth == 0 && end.local_name().as_ref() == b"pkgmetadata" {
                    break;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

fn read_use_flags(reader: &mut Reader<&[u8]>, use_flags: &mut Vec<UseFlagDescription>) {
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if depth == 0 && is_element(&start, b"flag") => {
                let name = flag_name(&start);
                let description = read_metadata_text(reader, &start).text;
                if let Some(name) = name.filter(|_| !description.is_empty()) {
                    use_flags.push(UseFlagDescription { name, description });
                }
            }
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(end)) => {
                if depth == 0 && end.local_name().as_ref() == b"use" {
                    break;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

fn read_metadata_text(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> MetadataText {
    let end_name = start.name().as_ref().to_vec();
    let lang = element_lang(start);
    let mut depth = 0usize;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::Text(event)) => append_text(&mut text, &event),
            Ok(Event::CData(event)) => append_cdata(&mut text, &event),
            Ok(Event::GeneralRef(event)) => append_general_ref(&mut text, &event),
            Ok(Event::End(end)) => {
                if depth == 0 && end.name().as_ref() == end_name.as_slice() {
                    break;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    MetadataText {
        lang,
        text: collapse_space(&text),
    }
}

fn append_text(output: &mut String, event: &BytesText<'_>) {
    if let Ok(decoded) = event.xml10_content() {
        output.push_str(&decoded);
    }
}

fn append_cdata(output: &mut String, event: &BytesCData<'_>) {
    if let Ok(decoded) = event.xml10_content() {
        output.push_str(&decoded);
    }
}

fn append_general_ref(output: &mut String, event: &BytesRef<'_>) {
    if let Ok(Some(ch)) = event.resolve_char_ref() {
        output.push(ch);
        return;
    }

    if let Ok(name) = event.xml10_content() {
        match name.as_ref() {
            "amp" => output.push('&'),
            "lt" => output.push('<'),
            "gt" => output.push('>'),
            "quot" => output.push('"'),
            "apos" => output.push('\''),
            _ => {
                output.push('&');
                output.push_str(&name);
                output.push(';');
            }
        }
    }
}

fn element_lang(start: &BytesStart<'_>) -> Option<String> {
    let mut attributes = start.attributes();
    attributes.with_checks(false);
    for attr in attributes.flatten() {
        if matches!(attr.key.as_ref(), b"lang" | b"xml:lang") {
            return attr
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|value| value.into_owned());
        }
    }
    None
}

fn flag_name(start: &BytesStart<'_>) -> Option<String> {
    let mut attributes = start.attributes();
    attributes.with_checks(false);
    for attr in attributes.flatten() {
        if attr.key.as_ref() == b"name" {
            return attr
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|value| collapse_space(&value))
                .filter(|value| !value.is_empty());
        }
    }
    None
}

fn is_element(start: &BytesStart<'_>, name: &[u8]) -> bool {
    start.local_name().as_ref() == name
}

fn render_rss(
    title: &str,
    repo_url: &str,
    self_url: Option<&str>,
    description: &str,
    items: &[PackageItem],
    show_author_email: bool,
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
        let html_description = item_description(item, &package_url, &commit_url, show_author_email);

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

fn item_description(
    item: &PackageItem,
    package_url: &str,
    commit_url: &str,
    show_author_email: bool,
) -> String {
    let mut parts = Vec::new();
    if !item.description.is_empty() {
        parts.push(format!(
            "<strong>{}</strong>",
            xml_escape(&item.description)
        ));
    }
    if !item.metadata_description.is_empty() && item.metadata_description != item.description {
        parts.push(format!(
            "Metadata description: <strong>{}</strong>",
            xml_escape(&item.metadata_description)
        ));
    }
    if !item.use_flags.is_empty() {
        parts.push("USE flags:".to_string());
        parts.extend(item.use_flags.iter().map(use_flag_html));
    }
    let commit_body = visible_commit_body(item);
    if (!item.commit_subject.is_empty() || commit_body.is_some()) && !parts.is_empty() {
        parts.push(String::new());
    }
    if !item.commit_subject.is_empty() {
        parts.push(format!(
            "Commit title: <a href=\"{}\">{}</a>",
            xml_escape(commit_url),
            xml_escape(&item.commit_subject)
        ));
    }
    if let Some(commit_body) = commit_body {
        parts.push(format!(
            "Commit body: {}",
            html_text_with_breaks(commit_body)
        ));
    }
    if let Some(author) = author_html(item, show_author_email) {
        parts.push(format!("Author: {author}"));
    }
    parts.push(format!(
        "Package: <a href=\"{}\">{}</a>",
        xml_escape(package_url),
        xml_escape(&item.package)
    ));
    parts.push(format!("Ebuild: {}", xml_escape(&item.ebuild_path)));
    if !item.distfiles.is_empty() {
        parts.push("Distfiles:".to_string());
        parts.extend(item.distfiles.iter().map(distfile_html));
    }
    if !item.patches.is_empty() {
        parts.push(format!("Patches: {}", patch_names_html(&item.patches)));
    }
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
    parts.join("<br/>\n")
}

fn visible_commit_body(item: &PackageItem) -> Option<&str> {
    if item.commit_body.is_empty()
        || is_redundant_signed_off_by_body(&item.commit_body, &item.author_name, &item.author_email)
    {
        None
    } else {
        Some(&item.commit_body)
    }
}

fn is_redundant_signed_off_by_body(body: &str, author_name: &str, author_email: &str) -> bool {
    if author_name.trim().is_empty() || author_email.trim().is_empty() {
        return false;
    }

    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }

    let Some((prefix, signed)) = line.split_once(':') else {
        return false;
    };
    if !prefix.trim().eq_ignore_ascii_case("Signed-off-by") {
        return false;
    }

    let Some((signed_name, signed_email)) = signed.trim().rsplit_once('<') else {
        return false;
    };
    let Some(signed_email) = signed_email.trim().strip_suffix('>') else {
        return false;
    };

    signed_name.trim() == author_name.trim()
        && signed_email
            .trim()
            .eq_ignore_ascii_case(author_email.trim())
}

fn author_html(item: &PackageItem, show_author_email: bool) -> Option<String> {
    let author_label = if item.author_name.is_empty() {
        item.author_github_username.as_str()
    } else {
        item.author_name.as_str()
    };
    let mut author = if !item.author_github_username.is_empty() {
        let profile_url = format!("https://github.com/{}", item.author_github_username);
        format!(
            "<a href=\"{}\">{}</a>",
            xml_escape(&profile_url),
            xml_escape(author_label)
        )
    } else if show_author_email && !item.author_name.is_empty() {
        xml_escape(&item.author_name)
    } else {
        String::new()
    };

    if show_author_email && !item.author_email.is_empty() {
        if !author.is_empty() {
            author.push(' ');
        }
        author.push_str(&format!("&lt;{}&gt;", xml_escape(&item.author_email)));
    }

    (!author.is_empty()).then_some(author)
}

fn patch_names_html(patches: &[String]) -> String {
    patches
        .iter()
        .map(|patch| format!("<code>{}</code>", xml_escape(patch)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn distfile_html(distfile: &ManifestDistfile) -> String {
    format!(
        "<code>{}</code>: {}",
        xml_escape(&distfile.name),
        human_size(distfile.size_bytes)
    )
}

fn human_size(size_bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;

    match size_bytes {
        0..=1023 => format!("{size_bytes} B"),
        1024..=1_048_575 => format!("{:.1} KB", size_bytes as f64 / KB),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", size_bytes as f64 / MB),
        _ => format!("{:.2} GB", size_bytes as f64 / GB),
    }
}

fn use_flag_html(flag: &UseFlagDescription) -> String {
    format!(
        "<strong>{}</strong>: {}",
        xml_escape(&flag.name),
        xml_escape(&flag.description)
    )
}

fn html_text_with_breaks(input: &str) -> String {
    xml_escape(input).replace('\n', "<br/>\n")
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
    fn extracts_github_username_from_noreply_email() {
        assert_eq!(
            github_username_from_noreply_email("123456+vitaly-zdanevich@users.noreply.github.com"),
            Some("vitaly-zdanevich".to_string())
        );
        assert_eq!(
            github_username_from_noreply_email("vitaly-zdanevich@users.noreply.github.com"),
            Some("vitaly-zdanevich".to_string())
        );
        assert_eq!(
            github_username_from_noreply_email("zdanevich.vitaly@ya.ru"),
            None
        );
        assert_eq!(
            github_username_from_noreply_email("-invalid@users.noreply.github.com"),
            None
        );
    }

    #[test]
    fn extracts_github_commit_author_login() {
        let json = r#"{
  "commit": {
    "author": {
      "name": "Leo Douglas",
      "email": "douglarek@gmail.com"
    }
  },
  "author": {
    "login": "douglarek",
    "html_url": "https://github.com/douglarek"
  },
  "committer": {
    "login": "peeweep"
  }
}"#;

        assert_eq!(
            github_author_login_from_commit_json(json),
            Some("douglarek".to_string())
        );
    }

    #[test]
    fn extracts_github_repo_from_url() {
        assert_eq!(
            github_repo_from_url("git@github.com:microcai/gentoo-zh.git"),
            Some(GitHubRepo {
                owner: "microcai".to_string(),
                name: "gentoo-zh".to_string(),
            })
        );
        assert_eq!(github_repo_from_url("https://gitlab.com/a/b"), None);
    }

    #[test]
    fn extracts_package_description_from_metadata_xml() {
        let metadata = r#"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE pkgmetadata SYSTEM "https://www.gentoo.org/dtd/metadata.dtd">
<pkgmetadata>
  <maintainer type="person">
    <email>maintainer@example.com</email>
    <description>Maintainer nickname</description>
  </maintainer>
  <description>Package &amp; summary</description>
  <longdescription lang="en">Fallback package text</longdescription>
  <use>
    <flag name="qt6">Use <pkg>dev-qt/qtbase</pkg> &amp; bindings</flag>
    <flag name="webengine"><![CDATA[Enable web login]]></flag>
  </use>
</pkgmetadata>
"#;

        assert_eq!(
            metadata_description(metadata),
            Some("Package & summary".to_string())
        );
        assert_eq!(
            package_metadata(metadata),
            PackageMetadata {
                description: Some("Package & summary".to_string()),
                use_flags: vec![
                    UseFlagDescription {
                        name: "qt6".to_string(),
                        description: "Use dev-qt/qtbase & bindings".to_string(),
                    },
                    UseFlagDescription {
                        name: "webengine".to_string(),
                        description: "Enable web login".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn ignores_maintainer_description_in_metadata_xml() {
        let metadata = r#"
<pkgmetadata>
  <maintainer type="person">
    <email>maintainer@example.com</email>
    <description>Maintainer nickname</description>
  </maintainer>
</pkgmetadata>
"#;

        assert_eq!(metadata_description(metadata), None);
    }

    #[test]
    fn prefers_english_longdescription_from_metadata_xml() {
        let metadata = r#"
<pkgmetadata>
  <longdescription lang="x-test">Wrong language text</longdescription>
  <longdescription lang="en">
    Fast <pkg>dev-util/foo</pkg> &amp; useful <![CDATA[tool]]>
  </longdescription>
</pkgmetadata>
"#;

        assert_eq!(
            metadata_description(metadata),
            Some("Fast dev-util/foo & useful tool".to_string())
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
    fn separates_commit_body_from_added_paths() {
        let (body, paths) = commit_body_and_added_paths(
            "Useful <details> & context\n\nSecond paragraph\n\nA\tdev-util/newpkg/metadata.xml",
        );

        assert_eq!(body, "Useful <details> & context\n\nSecond paragraph");
        assert_eq!(paths, vec!["dev-util/newpkg/metadata.xml"]);
    }

    #[test]
    fn detects_redundant_signed_off_by_body() {
        assert!(is_redundant_signed_off_by_body(
            "Signed-off-by: Leo Douglas <douglarek@gmail.com>",
            "Leo Douglas",
            "douglarek@gmail.com"
        ));
        assert!(is_redundant_signed_off_by_body(
            "\nSigned-off-by: Leo Douglas <DOUGLAREK@gmail.com>\n",
            "Leo Douglas",
            "douglarek@gmail.com"
        ));
        assert!(!is_redundant_signed_off_by_body(
            "Useful detail\n\nSigned-off-by: Leo Douglas <douglarek@gmail.com>",
            "Leo Douglas",
            "douglarek@gmail.com"
        ));
        assert!(!is_redundant_signed_off_by_body(
            "Signed-off-by: Another Person <douglarek@gmail.com>",
            "Leo Douglas",
            "douglarek@gmail.com"
        ));
        assert!(!is_redundant_signed_off_by_body(
            "Signed-off-by: Leo Douglas <douglarek@gmail.com>\nSigned-off-by: jinqiang zhang <jinqiang@zhang.my>",
            "Leo Douglas",
            "douglarek@gmail.com"
        ));
    }

    #[test]
    fn extracts_distfiles_from_manifest() {
        let distfiles = manifest_distfiles(
            r#"
DIST newpkg-1.tar.gz 1536 BLAKE2B abc SHA512 def
DIST large-source.tar.xz 2097152 BLAKE2B abc SHA512 def
EBUILD newpkg-1.ebuild 123 BLAKE2B abc SHA512 def
DIST broken-size not-a-number BLAKE2B abc SHA512 def
"#,
        );

        assert_eq!(
            distfiles,
            vec![
                ManifestDistfile {
                    name: "newpkg-1.tar.gz".to_string(),
                    size_bytes: 1536,
                },
                ManifestDistfile {
                    name: "large-source.tar.xz".to_string(),
                    size_bytes: 2_097_152,
                },
            ]
        );
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(2_097_152), "2.0 MB");
        assert_eq!(human_size(1_316_010_876), "1.23 GB");
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
        repo.commit_with_body(
            "app-existing/existing: add metadata",
            "Signed-off-by: Test User <123456+test-user@users.noreply.github.com>",
            "2024-01-04T00:00:00Z",
        );

        repo.write(
            "dev-util/newpkg/metadata.xml",
            r#"<pkgmetadata>
  <longdescription lang="en">Metadata &amp; package details</longdescription>
  <use>
    <flag name="qt6">Use <pkg>dev-qt/qtbase</pkg> &amp; bindings</flag>
    <flag name="webengine">Enable web login</flag>
  </use>
</pkgmetadata>
"#,
        );
        repo.write(
            "dev-util/newpkg/newpkg-1.ebuild",
            r#"DESCRIPTION="Useful & fast"
HOMEPAGE="https://new.example/path?x=1&y=2"
LICENSE="Apache-2.0"
"#,
        );
        repo.write(
            "dev-util/newpkg/Manifest",
            r#"
DIST newpkg-1.tar.gz 1536 BLAKE2B abc SHA512 def
DIST large-source.tar.xz 2097152 BLAKE2B abc SHA512 def
EBUILD newpkg-1.ebuild 123 BLAKE2B abc SHA512 def
"#,
        );
        repo.write("dev-util/newpkg/files/fix-build.patch", "patch content\n");
        repo.write(
            "dev-util/newpkg/files/subdir/fix-runtime.diff",
            "diff content\n",
        );
        repo.write("dev-util/newpkg/files/readme.txt", "not a patch\n");
        repo.commit_with_body(
            "dev-util/newpkg: new package",
            "Useful <details> & context\n\nSecond paragraph",
            "2024-01-05T00:00:00Z",
        );

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
            list_patches: false,
            github_author_links: false,
            show_author_email: true,
        })
        .expect("RSS generated");

        assert_eq!(generated, output);
        let rss = fs::read_to_string(output).expect("RSS can be read");
        assert!(rss.contains("<title>test-overlay: new Gentoo packages</title>"));
        assert!(rss.contains("dev-util/newpkg: new package"));
        assert!(rss.contains("<strong>Useful &amp; fast</strong>"));
        assert!(
            rss.contains("Metadata description: <strong>Metadata &amp; package details</strong>")
        );
        assert!(
            rss.contains("USE flags:<br/>\n<strong>qt6</strong>: Use dev-qt/qtbase &amp; bindings")
        );
        assert!(rss.contains("<strong>webengine</strong>: Enable web login"));
        assert!(
            rss.contains("<strong>webengine</strong>: Enable web login<br/>\n<br/>\nCommit title:")
        );
        assert!(rss.contains("Commit title: <a href=\"https://github.com/example/overlay/commit/"));
        assert!(rss.contains("\">dev-util/newpkg: new package</a>"));
        assert!(rss.contains(
            "Commit body: Useful &lt;details&gt; &amp; context<br/>\n<br/>\nSecond paragraph"
        ));
        assert!(rss.contains(
            "Author: <a href=\"https://github.com/test-user\">Test User</a> &lt;123456+test-user@users.noreply.github.com&gt;"
        ));
        assert!(rss.contains("Package: <a href=\"https://github.com/example/overlay/tree/"));
        assert!(rss.contains("/dev-util/newpkg\">dev-util/newpkg</a>"));
        assert!(rss.contains(
            "Distfiles:<br/>\n<code>newpkg-1.tar.gz</code>: 1.5 KB<br/>\n<code>large-source.tar.xz</code>: 2.0 MB"
        ));
        assert!(!rss.contains("Patches:"));
        assert!(!rss.contains("Package directory"));
        assert!(!rss.contains(">Commit</a>"));
        assert!(rss.contains("app-existing/existing: add metadata"));
        assert!(!rss.contains("Commit body: Signed-off-by: Test User"));
        assert!(rss.contains("https://github.com/example/overlay/commit/"));
        assert!(!rss.contains("initial import"));
        assert!(!rss.contains("app-root/rootpkg: add 2"));

        let output = repo.path.join("public/feed-with-patches.rss");
        generate(Config {
            repo: repo.path.clone(),
            output: Some(output.clone()),
            repo_url: Some("git@github.com:example/overlay.git".to_string()),
            self_url: Some("https://example.github.io/overlay/feed-with-patches.rss".to_string()),
            title: None,
            description: None,
            max_items: None,
            include_root: false,
            list_patches: true,
            github_author_links: false,
            show_author_email: false,
        })
        .expect("RSS generated with patch names");
        let rss = fs::read_to_string(output).expect("RSS can be read");
        assert!(rss.contains(
            "Patches: <code>fix-build.patch</code>, <code>subdir/fix-runtime.diff</code>"
        ));
        assert!(rss.contains("Author: <a href=\"https://github.com/test-user\">Test User</a>"));
        assert!(!rss.contains("123456+test-user@users.noreply.github.com&gt;"));
        assert!(!rss.contains("readme.txt"));
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
            self.commit_with_body(message, "", date);
        }

        fn commit_with_body(&self, message: &str, body: &str, date: &str) {
            self.git(["add", "."], None);
            let mut args = vec!["commit", "-m", message];
            if !body.is_empty() {
                args.extend(["-m", body]);
            }
            self.git(args, Some(date));
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
                .arg("user.email=123456+test-user@users.noreply.github.com");
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
