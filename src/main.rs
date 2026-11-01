use anyhow::{Context, Result, bail};
use clap::{Parser as CliParser, Subcommand};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as Input, KeyCode, KeyEventKind,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::{Command, Stdio},
};
use unicode_width::UnicodeWidthChar;

#[derive(CliParser)]
#[command(version, about = "Read and convert the current directory's README")]
struct Cli {
    /// Select a Markdown file explicitly
    file: Option<PathBuf>,
    #[arg(long = "static", conflicts_with_all = ["json", "yaml", "html", "toml"])]
    static_output: bool,
    #[arg(long, conflicts_with_all = ["yaml", "html", "toml"])]
    json: bool,
    #[arg(long, conflicts_with_all = ["html", "toml"])]
    yaml: bool,
    #[arg(long, conflicts_with = "toml")]
    html: bool,
    #[arg(long)]
    toml: bool,
    /// Show literal Markdown; with --html escape it inside a preformatted page
    #[arg(long)]
    raw: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Install a local executable Markdown filter
    Install { executable: PathBuf },
    /// List installed extensions
    Extensions,
    /// Remove an installed extension
    Uninstall { name: String },
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    heading_color: Option<String>,
    text_color: Option<String>,
    html_css: Option<String>,
    extensions: Vec<String>,
}
#[derive(Serialize)]
struct Document {
    source: String,
    markdown: String,
    headings: Vec<Heading>,
}
#[derive(Clone, Serialize)]
struct Heading {
    level: u8,
    text: String,
}
#[derive(Clone)]
struct Row {
    text: String,
    heading: Option<usize>,
    style: Style,
}

fn root() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("Cannot determine configuration directory")?
        .join("readme"))
}
fn config() -> Result<Config> {
    let p = root()?.join("config.toml");
    if p.exists() {
        Ok(toml::from_str(&fs::read_to_string(&p)?)
            .with_context(|| format!("Invalid config: {}", p.display()))?)
    } else {
        Ok(Config::default())
    }
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
fn extension_dir() -> Result<PathBuf> {
    Ok(root()?.join("extensions"))
}
fn extension_command(command: Commands) -> Result<()> {
    let dir = extension_dir()?;
    match command {
        Commands::Install { executable } => {
            let name = executable
                .file_name()
                .and_then(|x| x.to_str())
                .context("Invalid extension name")?;
            if !valid_name(name) {
                bail!("Extension name may contain only letters, numbers, '-' and '_'")
            }
            if !executable.is_file() {
                bail!("Extension must be a file")
            }
            fs::create_dir_all(&dir)?;
            let dest = dir.join(name);
            if dest.exists() {
                bail!("Extension already exists: {}", dest.display())
            }
            fs::copy(&executable, &dest)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
            }
            eprintln!(
                "Installed {name}; add it to extensions in {} to enable it",
                root()?.join("config.toml").display()
            );
        }
        Commands::Extensions => {
            if dir.exists() {
                let mut names = fs::read_dir(dir)?
                    .filter_map(|x| x.ok())
                    .map(|x| x.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                names.sort();
                for n in names {
                    println!("{n}");
                }
            }
        }
        Commands::Uninstall { name } => {
            if !valid_name(&name) {
                bail!("Invalid extension name")
            }
            fs::remove_file(dir.join(name))?;
        }
    }
    Ok(())
}
fn choose_file(path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let mut files = fs::read_dir(".")?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("readme.md")
        })
        .map(|e| e.path())
        .collect::<Vec<_>>();
    files.sort();
    match files.len() {
        0 => bail!("No README.md found in the current directory"),
        1 => Ok(files.remove(0)),
        _ => {
            if !io::stdin().is_terminal() {
                bail!("Multiple README files found; specify a file explicitly")
            }
            eprintln!("Multiple README files found:");
            for (i, f) in files.iter().enumerate() {
                eprintln!("  {}. {}", i + 1, f.display());
            }
            loop {
                eprint!("Choose a number: ");
                io::stderr().flush()?;
                let mut s = String::new();
                if io::stdin().read_line(&mut s)? == 0 {
                    bail!("No selection")
                }
                if let Ok(n) = s.trim().parse::<usize>()
                    && n > 0
                    && n <= files.len()
                {
                    return Ok(files.remove(n - 1));
                }
                eprintln!("Invalid selection");
            }
        }
    }
}
fn apply_extensions(mut markdown: String, config: &Config) -> Result<String> {
    for name in &config.extensions {
        if !valid_name(name) {
            bail!("Invalid extension name: {name}")
        }
        let path = extension_dir()?.join(name);
        let mut child = Command::new(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("Cannot run extension {}", path.display()))?;
        let mut input = child.stdin.take().context("Cannot open extension stdin")?;
        let writer = std::thread::spawn(move || input.write_all(markdown.as_bytes()));
        let output = child.wait_with_output()?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("Extension input thread failed"))??;
        if !output.status.success() {
            bail!("Extension {name} failed: {}", output.status)
        }
        markdown = String::from_utf8(output.stdout)
            .with_context(|| format!("Extension {name} produced non-UTF-8 output"))?;
    }
    Ok(markdown)
}
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}
fn level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
fn color(s: Option<&str>, fallback: Color) -> Color {
    match s.unwrap_or("").to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        _ => fallback,
    }
}
// Markdown inside HTML containers is often emitted as an HTML block. Strip the
// tags only from HTML events, then parse the resulting Markdown again.
fn terminal_markdown(md: &str) -> String {
    let tags = regex::Regex::new(r"(?s)<!--.*?-->|</?[A-Za-z][^>]*>").unwrap();
    let img = regex::Regex::new(r#"(?is)<img\b[^>]*\balt\s*=\s*["']([^"']*)["'][^>]*>"#).unwrap();
    let mut result = String::new();
    let mut offset = 0;
    for (event, range) in Parser::new_ext(md, options()).into_offset_iter() {
        if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            if range.start < offset {
                continue;
            }
            result.push_str(&md[offset..range.start]);
            let fragment = &md[range.clone()];
            let fragment = img.replace_all(fragment, "[image: $1]");
            result.push_str(&tags.replace_all(&fragment, ""));
            offset = range.end;
        }
    }
    result.push_str(&md[offset..]);
    result
}
fn wrap_rows(rows: &[Row], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut result = Vec::new();
    for row in rows {
        let mut text = String::new();
        let mut used = 0;
        for ch in row.text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > width && !text.is_empty() {
                result.push(Row {
                    text: std::mem::take(&mut text),
                    ..row.clone()
                });
                used = 0;
            }
            text.push(ch);
            used += w;
        }
        result.push(Row {
            text,
            ..row.clone()
        });
    }
    result
}
fn render(md: &str, cfg: &Config) -> (Vec<Row>, Vec<Heading>) {
    let mut rows = Vec::new();
    let mut headings = Vec::new();
    let mut line = String::new();
    let mut current = None;
    let mut code = false;
    let mut list = 0usize;
    let normal = Style::default().fg(color(cfg.text_color.as_deref(), Color::White));
    let title = Style::default()
        .fg(color(cfg.heading_color.as_deref(), Color::Cyan))
        .add_modifier(Modifier::BOLD);
    let flush = |rows: &mut Vec<Row>, line: &mut String, current: Option<usize>| {
        rows.push(Row {
            text: std::mem::take(line),
            heading: current,
            style: if current.is_some() { title } else { normal },
        });
    };
    let prepared = terminal_markdown(md);
    for event in Parser::new_ext(&prepared, options()) {
        match event {
            Event::Start(Tag::Heading { level: l, .. }) => {
                if !line.is_empty() {
                    flush(&mut rows, &mut line, None);
                }
                if !rows.is_empty() && rows.last().is_some_and(|r: &Row| !r.text.is_empty()) {
                    flush(&mut rows, &mut line, None);
                }
                current = Some(headings.len());
                headings.push(Heading {
                    level: level(l),
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(i) = current {
                    headings[i].text = line.clone();
                }
                flush(&mut rows, &mut line, current);
                current = None;
            }
            Event::Start(Tag::Image { .. }) => line.push_str("[image: "),
            Event::End(TagEnd::Image) => line.push(']'),
            Event::Start(Tag::CodeBlock(_)) => {
                code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                if !line.is_empty() {
                    flush(&mut rows, &mut line, None);
                }
                code = false;
            }
            Event::Start(Tag::List(_)) => list += 1,
            Event::End(TagEnd::List(_)) => list = list.saturating_sub(1),
            Event::Start(Tag::Item) => {
                line.push_str(&"  ".repeat(list.saturating_sub(1)));
                line.push_str("• ");
            }
            Event::Start(Tag::BlockQuote(_)) => line.push_str("│ "),
            Event::Text(s) | Event::Code(s) | Event::Html(s) | Event::InlineHtml(s) => {
                for (i, part) in s.split('\n').enumerate() {
                    if i > 0 {
                        flush(&mut rows, &mut line, current);
                    }
                    if code && line.is_empty() {
                        line.push_str("    ");
                    }
                    line.push_str(part);
                }
            }
            Event::SoftBreak => line.push(' '),
            Event::HardBreak => flush(&mut rows, &mut line, current),
            Event::Rule => {
                if !line.is_empty() {
                    flush(&mut rows, &mut line, current);
                }
                line.push_str("────────────────────");
                flush(&mut rows, &mut line, None);
            }
            Event::TaskListMarker(done) => line.push_str(if done { "[x] " } else { "[ ] " }),
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::TableRow | TagEnd::TableHead) => {
                if !line.is_empty() {
                    flush(&mut rows, &mut line, current);
                }
                if matches!(event, Event::End(TagEnd::Paragraph)) && list == 0 {
                    flush(&mut rows, &mut line, None);
                }
            }
            Event::End(TagEnd::TableCell) => line.push_str("  │  "),
            Event::FootnoteReference(s) => line.push_str(&format!("[{s}]")),
            _ => {}
        }
    }
    if !line.is_empty() {
        flush(&mut rows, &mut line, current);
    }
    (rows, headings)
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn html_document(md: &str, raw: bool, cfg: &Config) -> String {
    let mut body = String::new();
    if raw {
        body = format!("<pre>{}</pre>", escape(md));
    } else {
        html::push_html(&mut body, Parser::new_ext(md, options()));
    }
    let css=cfg.html_css.as_deref().unwrap_or("body{max-width:860px;margin:3em auto;padding:0 1em;font:16px/1.65 system-ui,sans-serif;color:#24292f}pre{overflow:auto;padding:1em;background:#f6f8fa}code{font-family:monospace}img{max-width:100%}table{border-collapse:collapse}td,th{border:1px solid #aaa;padding:.4em}blockquote{border-left:4px solid #ccc;padding-left:1em;color:#57606a}");
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>README</title><style>{css}</style></head><body>\n{body}\n</body></html>\n"
    )
}
fn visible(rows: &[Row], headings: &[Heading], folded: &HashSet<usize>) -> Vec<Row> {
    let mut out = Vec::new();
    let mut hidden: Option<u8> = None;
    for row in rows {
        if let Some(i) = row.heading {
            let l = headings[i].level;
            if hidden.is_some_and(|h| l > h) {
                continue;
            }
            hidden = None;
            let mut r = row.clone();
            r.text = format!("{} {}", if folded.contains(&i) { "▸" } else { "▾" }, r.text);
            out.push(r);
            if folded.contains(&i) {
                hidden = Some(l);
            }
        } else if hidden.is_none() {
            out.push(row.clone());
        }
    }
    out
}
fn tui(rows: Vec<Row>, headings: Vec<Heading>) -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        bail!("Interactive mode requires a terminal; use --static for redirected output")
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = (|| -> Result<()> {
        let mut folded = HashSet::new();
        let mut scroll = 0usize;
        let mut selected = 0usize;
        let mut outline = false;
        loop {
            let size = terminal.size()?;
            let shown = wrap_rows(
                &visible(&rows, &headings, &folded),
                size.width.saturating_sub(2) as usize,
            );
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(f.area());
                let lines: Vec<Line> = if outline {
                    headings
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            Line::from(Span::styled(
                                format!(
                                    "{}{}{}",
                                    if i == selected { "› " } else { "  " },
                                    "  ".repeat(h.level.saturating_sub(1) as usize),
                                    h.text
                                ),
                                if i == selected {
                                    Style::default()
                                        .fg(Color::Yellow)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default()
                                },
                            ))
                        })
                        .collect()
                } else {
                    shown
                        .iter()
                        .map(|r| Line::from(Span::styled(r.text.clone(), r.style)))
                        .collect()
                };
                let offset = if outline {
                    selected.saturating_sub(chunks[0].height.saturating_sub(3) as usize)
                } else {
                    scroll
                };
                f.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title(if outline {
                            "Outline"
                        } else {
                            "README"
                        }))
                        .scroll((offset.min(u16::MAX as usize) as u16, 0)),
                    chunks[0],
                );
                f.render_widget(
                    Paragraph::new(
                        "↑↓/jk scroll  n/p heading  click/Enter fold  o outline  q quit",
                    ),
                    chunks[1],
                );
            })?;
            match event::read()? {
                Input::Mouse(mouse) if !outline => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.column > 0
                            && mouse.column < size.width.saturating_sub(1)
                            && mouse.row > 0
                            && mouse.row < size.height.saturating_sub(2) =>
                    {
                        if let Some(i) = shown
                            .get(scroll + mouse.row as usize - 1)
                            .and_then(|r| r.heading)
                        {
                            if !folded.insert(i) {
                                folded.remove(&i);
                            }
                            scroll = scroll.min(shown.len().saturating_sub(1));
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        scroll = (scroll + 3).min(shown.len().saturating_sub(1))
                    }
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                    _ => {}
                },
                Input::Key(k) => {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if outline {
                                outline = false
                            } else {
                                break;
                            }
                        }
                        KeyCode::Char('o') => outline = !outline,
                        KeyCode::Char('n') if !outline => {
                            if let Some(pos) = shown
                                .iter()
                                .enumerate()
                                .skip(scroll + 1)
                                .find(|(i, r)| {
                                    r.heading.is_some()
                                        && (*i == 0 || shown[*i - 1].heading != r.heading)
                                })
                                .map(|(i, _)| i)
                            {
                                scroll = pos;
                            }
                        }
                        KeyCode::Char('p') if !outline => {
                            if let Some(pos) = shown
                                .iter()
                                .enumerate()
                                .take(scroll)
                                .rev()
                                .find(|(i, r)| {
                                    r.heading.is_some()
                                        && (*i == 0 || shown[*i - 1].heading != r.heading)
                                })
                                .map(|(i, _)| i)
                            {
                                scroll = pos;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if outline {
                                selected = (selected + 1).min(headings.len().saturating_sub(1))
                            } else {
                                scroll = (scroll + 1).min(shown.len().saturating_sub(1))
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if outline {
                                selected = selected.saturating_sub(1)
                            } else {
                                scroll = scroll.saturating_sub(1)
                            }
                        }
                        KeyCode::PageDown => {
                            scroll = (scroll + 10).min(shown.len().saturating_sub(1))
                        }
                        KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            if outline {
                                if !headings.is_empty() {
                                    folded.clear();
                                    let all = wrap_rows(
                                        &visible(&rows, &headings, &folded),
                                        size.width.saturating_sub(2) as usize,
                                    );
                                    scroll = all
                                        .iter()
                                        .position(|r| r.heading == Some(selected))
                                        .unwrap_or(0);
                                    outline = false;
                                }
                            } else if let Some(i) = shown.get(scroll).and_then(|r| r.heading)
                                && !folded.insert(i)
                            {
                                folded.remove(&i);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let raw = disable_raw_mode();
    let leave = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
    result?;
    raw?;
    leave?;
    Ok(())
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(cmd) = cli.command {
        return extension_command(cmd);
    }
    let path = choose_file(cli.file)?;
    let cfg = config()?;
    let original = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {} as UTF-8", path.display()))?;
    let markdown = if cli.raw {
        original
    } else {
        apply_extensions(original, &cfg)?
    };
    let (rows, headings) = render(&markdown, &cfg);
    if cli.html {
        print!("{}", html_document(&markdown, cli.raw, &cfg));
    } else if cli.json || cli.yaml || cli.toml {
        let doc = Document {
            source: path.display().to_string(),
            markdown,
            headings,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&doc)?)
        } else if cli.yaml {
            print!("{}", serde_yaml::to_string(&doc)?)
        } else {
            print!("{}", toml::to_string_pretty(&doc)?)
        }
    } else if cli.raw {
        print!("{markdown}")
    } else if cli.static_output {
        for row in rows {
            println!("{}", row.text)
        }
    } else {
        tui(rows, headings)?
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("readme: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_containers_do_not_hide_markdown_or_leak_tags() {
        let (rows, headings) = render(
            "<div align=\"center\">\n\n# Hello\n\nSome **text**.\n\n</div>\n",
            &Config::default(),
        );
        assert_eq!(headings[0].text, "Hello");
        let text = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Some text."));
        assert!(!text.contains("<div"));
        let (rows, _) = render("```html\n<div>\n```", &Config::default());
        assert!(rows.iter().any(|r| r.text.contains("<div>")));
    }
    #[test]
    fn wrapped_rows_keep_heading_mapping() {
        let row = Row {
            text: "▾ 中文 heading".into(),
            heading: Some(0),
            style: Style::default(),
        };
        let wrapped = wrap_rows(&[row], 5);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|r| r.heading == Some(0)));
    }
    #[test]
    fn folding_hides_nested_content_but_not_siblings() {
        let (rows, headings) = render(
            "# First\nA\n## Nested\nB\n# Second\nC\n",
            &Config::default(),
        );
        let shown = visible(&rows, &headings, &HashSet::from([0]));
        let text = shown
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("First") && text.contains("Second") && text.contains("C"));
        assert!(!text.contains("Nested") && !text.contains("B"));
    }
    #[test]
    fn html_raw_escapes_source_and_rendered_html_parses_markdown() {
        let cfg = Config::default();
        assert!(html_document("# <tag>", true, &cfg).contains("# &lt;tag&gt;"));
        assert!(html_document("# Hello", false, &cfg).contains("<h1>Hello</h1>"));
    }
    #[test]
    fn structured_formats_preserve_content() {
        let doc = Document {
            source: "README.md".into(),
            markdown: "# Hello\n".into(),
            headings: vec![Heading {
                level: 1,
                text: "Hello".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&doc).unwrap()["headings"][0]["text"],
            "Hello"
        );
        assert!(serde_yaml::to_string(&doc).unwrap().contains("Hello"));
        assert!(toml::to_string(&doc).unwrap().contains("Hello"));
    }
}
