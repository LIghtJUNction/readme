use anyhow::{Context, Result, bail};
use clap::{Parser as CliParser, Subcommand, ValueEnum};
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
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
    sliced::{SlicedImage, SlicedProtocol},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
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
    /// Image protocol: auto, kitty, iterm2, sixel or blocks
    #[arg(long, value_enum, default_value_t = ImageProtocol::Auto)]
    image_protocol: ImageProtocol,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Clone, Copy, ValueEnum)]
enum ImageProtocol {
    Auto,
    Kitty,
    Iterm2,
    Sixel,
    Blocks,
}
fn picker_for(choice: ImageProtocol, has_images: bool) -> Picker {
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let explicit = match choice {
        ImageProtocol::Kitty => Some(ProtocolType::Kitty),
        ImageProtocol::Iterm2 => Some(ProtocolType::Iterm2),
        ImageProtocol::Sixel => Some(ProtocolType::Sixel),
        ImageProtocol::Blocks => Some(ProtocolType::Halfblocks),
        ImageProtocol::Auto
            if std::env::var_os("WEZTERM_EXECUTABLE").is_some()
                || program == "wezterm"
                || program.contains("iterm") =>
        {
            Some(ProtocolType::Iterm2)
        }
        ImageProtocol::Auto
            if term.contains("kitty")
                || term.contains("ghostty")
                || program.contains("ghostty")
                || std::env::var_os("KITTY_WINDOW_ID").is_some() =>
        {
            Some(ProtocolType::Kitty)
        }
        ImageProtocol::Auto if term.starts_with("foot") => Some(ProtocolType::Sixel),
        ImageProtocol::Auto => None,
    };
    if let Some(protocol) = explicit {
        let w = crossterm::terminal::window_size().ok();
        let cw = w
            .as_ref()
            .filter(|s| s.width > 0 && s.columns > 0)
            .map_or(10, |s| (s.width / s.columns).max(1));
        let ch = w
            .as_ref()
            .filter(|s| s.height > 0 && s.rows > 0)
            .map_or(20, |s| (s.height / s.rows).max(1));
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(ratatui_image::FontSize::new(cw, ch));
        picker.set_protocol_type(protocol);
        picker
    } else if has_images {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    } else {
        Picker::halfblocks()
    }
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
    image: Option<Arc<image::RgbaImage>>,
    pixels: Option<Vec<(Color, Color)>>,
    native: Option<NativeRow>,
}
#[derive(Clone)]
struct NativeRow {
    id: usize,
    offset: u32,
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
    let img = regex::Regex::new(r"(?is)<img\b[^>]*>").unwrap();
    let src = regex::Regex::new(r#"(?i)\bsrc\s*=\s*["']([^"']+)["']"#).unwrap();
    let alt = regex::Regex::new(r#"(?i)\balt\s*=\s*["']([^"']*)["']"#).unwrap();
    let mut result = String::new();
    let mut offset = 0;
    for (event, range) in Parser::new_ext(md, options()).into_offset_iter() {
        if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            if range.start < offset {
                continue;
            }
            result.push_str(&md[offset..range.start]);
            let fragment = &md[range.clone()];
            let fragment = img.replace_all(fragment, |caps: &regex::Captures| {
                let tag = &caps[0];
                match src.captures(tag) {
                    Some(source) => format!(
                        "![{}]({})",
                        alt.captures(tag)
                            .map_or("image", |a| a.get(1).unwrap().as_str()),
                        source.get(1).unwrap().as_str().replace("&amp;", "&")
                    ),
                    None => String::new(),
                }
            });
            result.push_str(&tags.replace_all(&fragment, ""));
            offset = range.end;
        }
    }
    result.push_str(&md[offset..]);
    result
}
const IMAGE_LIMIT: u64 = 16 * 1024 * 1024;
fn load_image(source: &str, base: &Path) -> Result<image::RgbaImage> {
    let mut bytes = Vec::new();
    if source.starts_with("https://") || source.starts_with("http://") {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build()
            .into();
        let mut response = agent.get(source).call().context("download failed")?;
        response
            .body_mut()
            .with_config()
            .limit(IMAGE_LIMIT)
            .reader()
            .read_to_end(&mut bytes)?;
    } else {
        let path = percent_encoding::percent_decode_str(
            source
                .split('#')
                .next()
                .unwrap_or(source)
                .split('?')
                .next()
                .unwrap_or(source),
        )
        .decode_utf8_lossy();
        let path = base.join(path.as_ref());
        let file =
            fs::File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        file.take(IMAGE_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > IMAGE_LIMIT {
            bail!("image exceeds 16 MiB");
        }
    }
    if let Ok(reader) = image::ImageReader::new(io::Cursor::new(&bytes)).with_guessed_format() {
        let mut reader = reader;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        if let Ok(decoded) = reader.decode() {
            return Ok(decoded.to_rgba8());
        }
    }
    let mut svg_options = resvg::usvg::Options::default();
    svg_options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(&bytes, &svg_options)
        .context("unsupported image format or invalid image")?;
    let size = tree.size().to_int_size();
    let scale = (2048.0 / size.width().max(size.height()) as f32).min(1.0);
    let width = (size.width() as f32 * scale).round().max(1.0) as u32;
    let height = (size.height() as f32 * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).context("invalid SVG size")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut raw = pixmap.take();
    // tiny-skia uses premultiplied alpha; image uses straight alpha.
    for p in raw.as_chunks_mut::<4>().0 {
        if p[3] != 0 {
            for i in 0..3 {
                p[i] = ((p[i] as u32 * 255 / p[3] as u32).min(255)) as u8;
            }
        }
    }
    image::RgbaImage::from_raw(width, height, raw).context("invalid SVG pixels")
}
fn image_rows(img: &image::RgbaImage, width: usize, row: &Row) -> Vec<Row> {
    let by_height = (80u64 * img.width() as u64 / img.height().max(1) as u64).max(1) as usize;
    let w = (img.width() as usize).min(width).min(by_height).max(1);
    let h =
        ((img.height() as f64 * w as f64 / img.width().max(1) as f64).round() as u32).clamp(1, 80);
    let resized = image::imageops::resize(img, w as u32, h, image::imageops::FilterType::Triangle);
    let pixel = |x: u32, y: u32| {
        let p = resized.get_pixel(x, y.min(h - 1));
        let a = p[3] as u16;
        let blend = |v: u8, bg: u8| ((v as u16 * a + bg as u16 * (255 - a)) / 255) as u8;
        Color::Rgb(blend(p[0], 24), blend(p[1], 26), blend(p[2], 32))
    };
    (0..h.div_ceil(2))
        .map(|y| Row {
            text: String::new(),
            image: None,
            pixels: Some(
                (0..w as u32)
                    .map(|x| (pixel(x, y * 2), pixel(x, y * 2 + 1)))
                    .collect(),
            ),
            ..row.clone()
        })
        .collect()
}
fn wrap_rows(rows: &[Row], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut result = Vec::new();
    for row in rows {
        if let Some(img) = &row.image {
            result.extend(image_rows(img, width, row));
            continue;
        }
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
fn native_wrap(rows: &[Row], width: usize, protocols: &HashMap<usize, SlicedProtocol>) -> Vec<Row> {
    let mut result = Vec::new();
    for row in rows {
        if let Some(img) = &row.image {
            let id = Arc::as_ptr(img) as usize;
            if let Some(protocol) = protocols.get(&id) {
                let size = protocol.size();
                for offset in 0..size.height {
                    result.push(Row {
                        text: String::new(),
                        image: None,
                        pixels: None,
                        native: Some(NativeRow {
                            id,
                            offset: offset as u32,
                        }),
                        ..row.clone()
                    });
                }
            } else {
                result.extend(wrap_rows(std::slice::from_ref(row), width));
            }
        } else {
            result.extend(wrap_rows(std::slice::from_ref(row), width));
        }
    }
    result
}
fn image_protocols(rows: &[Row], width: u16, picker: &Picker) -> HashMap<usize, SlicedProtocol> {
    let mut protocols = HashMap::new();
    for row in rows {
        if let Some(img) = &row.image {
            let id = Arc::as_ptr(img) as usize;
            if protocols.contains_key(&id) {
                continue;
            }
            let size = picker.font_size();
            let cols = (img.width() as f64 / size.width.max(1) as f64)
                .ceil()
                .clamp(1.0, width.max(1) as f64) as u16;
            let h = (img.height() as f64 * cols as f64 * size.width as f64
                / img.width().max(1) as f64
                / size.height.max(1) as f64)
                .ceil()
                .clamp(1.0, 200.0) as u16;
            let image = image::DynamicImage::ImageRgba8((**img).clone());
            if let Ok(protocol) = SlicedProtocol::new_with_resize(
                picker,
                image,
                ratatui::layout::Size::new(cols, h),
                Resize::Fit(None),
            ) {
                protocols.insert(id, protocol);
            }
        }
    }
    protocols
}
fn render(md: &str, cfg: &Config, base: &Path, images: bool) -> (Vec<Row>, Vec<Heading>) {
    let mut rows = Vec::new();
    let mut headings = Vec::new();
    let mut line = String::new();
    let mut current = None;
    let mut code = false;
    let mut list = 0usize;
    let mut pending_image: Option<(String, String)> = None;
    let normal = Style::default().fg(color(cfg.text_color.as_deref(), Color::White));
    let title = Style::default()
        .fg(color(cfg.heading_color.as_deref(), Color::Cyan))
        .add_modifier(Modifier::BOLD);
    let flush = |rows: &mut Vec<Row>, line: &mut String, current: Option<usize>| {
        let row_style = if current.is_some() {
            title
        } else if line.starts_with("    ") {
            Style::default().fg(Color::Yellow)
        } else {
            normal
        };
        rows.push(Row {
            text: std::mem::take(line),
            heading: current,
            style: row_style,
            image: None,
            pixels: None,
            native: None,
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
            Event::Start(Tag::Image { dest_url, .. }) => {
                if !line.is_empty() {
                    flush(&mut rows, &mut line, current);
                }
                pending_image = Some((dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((source, alt)) = pending_image.take() {
                    let text = format!("[image: {}]", if alt.is_empty() { &source } else { &alt });
                    let mut row = Row {
                        text,
                        heading: None,
                        style: Style::default().fg(Color::DarkGray),
                        image: None,
                        pixels: None,
                        native: None,
                    };
                    if images {
                        match load_image(&source, base) {
                            Ok(img) => row.image = Some(Arc::new(img)),
                            Err(e) => row.text = format!("{} ({}: {e:#})", row.text, source),
                        }
                    }
                    rows.push(row);
                }
            }
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
                if let Some((_, alt)) = &mut pending_image {
                    alt.push_str(&s);
                    continue;
                }
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
fn tui(rows: Vec<Row>, headings: Vec<Heading>, choice: ImageProtocol) -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        bail!("Interactive mode requires a terminal; use --static for redirected output")
    }
    // Images need color to be distinguishable; the interactive viewer is graphical.
    crossterm::style::Colored::set_ansi_color_disabled(false);
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let picker = picker_for(choice, rows.iter().any(|r| r.image.is_some()));
    let result = (|| -> Result<()> {
        let mut folded = HashSet::new();
        let mut scroll = 0usize;
        let mut selected = 0usize;
        let mut outline = false;
        let mut protocols: HashMap<usize, SlicedProtocol> = HashMap::new();
        let mut protocol_width = 0;
        let mut cached: Vec<Row> = Vec::new();
        let mut cached_width = 0;
        let mut cached_folded = HashSet::new();
        loop {
            let size = terminal.size()?;
            let width = size.width.saturating_sub(2) as usize;
            if protocol_width != width {
                protocols = image_protocols(&rows, width as u16, &picker);
                protocol_width = width;
            }
            if cached_width != width || cached_folded != folded || cached.is_empty() {
                cached = native_wrap(&visible(&rows, &headings, &folded), width, &protocols);
                cached_width = width;
                cached_folded = folded.clone();
            }
            if !cached.is_empty() {
                scroll = scroll.min(cached.len() - 1);
            } else {
                scroll = 0;
            }
            let shown = &cached;
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
                        .map(|r| match &r.pixels {
                            Some(pixels) => Line::from(
                                pixels
                                    .iter()
                                    .map(|(fg, bg)| {
                                        Span::styled("▀", Style::default().fg(*fg).bg(*bg))
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                            None => Line::from(Span::styled(r.text.clone(), r.style)),
                        })
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
                if !outline {
                    let inner = ratatui::layout::Rect::new(
                        chunks[0].x + 1,
                        chunks[0].y + 1,
                        chunks[0].width.saturating_sub(2),
                        chunks[0].height.saturating_sub(2),
                    );
                    for (index, row) in shown.iter().enumerate() {
                        if let Some(native) = &row.native
                            && native.offset == 0
                        {
                            let y = (index as i64 - scroll as i64)
                                .clamp(i16::MIN as i64, i16::MAX as i64)
                                as i16;
                            if let Some(protocol) = protocols.get(&native.id) {
                                f.render_widget(SlicedImage::new(protocol, (0, y).into()), inner);
                            }
                        }
                    }
                }
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
                                    let all = native_wrap(
                                        &visible(&rows, &headings, &folded),
                                        size.width.saturating_sub(2) as usize,
                                        &protocols,
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
    let interactive =
        !cli.html && !cli.json && !cli.yaml && !cli.toml && !cli.raw && !cli.static_output;
    let (rows, headings) = render(
        &markdown,
        &cfg,
        path.parent().unwrap_or(Path::new(".")),
        interactive,
    );
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
        tui(rows, headings, cli.image_protocol)?
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
    fn images_render_as_colored_cells_and_follow_folding() {
        let md = "# Top\n![shot](assets/readme-viewer.png)\n# End\n";
        let (rows, headings) = render(md, &Config::default(), Path::new("."), true);
        assert!(rows.iter().any(|r| r.image.is_some()));
        let wrapped = wrap_rows(&rows, 40);
        assert!(
            wrapped
                .iter()
                .any(|r| r.pixels.as_ref().is_some_and(|p| !p.is_empty()))
        );
        assert!(
            !visible(&rows, &headings, &HashSet::from([0]))
                .iter()
                .any(|r| r.image.is_some())
        );
        let (missing, _) = render(
            "![missing](missing.png)",
            &Config::default(),
            Path::new("."),
            true,
        );
        assert!(missing.iter().any(|r| r.text.contains("cannot open")));
    }
    #[test]
    fn html_image_and_svg_are_rendered() {
        let name = format!("readme-test-{}.svg", std::process::id());
        let path = std::env::temp_dir().join(&name);
        fs::write(&path, "<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'><rect width='4' height='4' fill='red'/></svg>").unwrap();
        let markdown = format!("<div><img alt='red' src='{name}'></div>");
        let (rows, _) = render(&markdown, &Config::default(), &std::env::temp_dir(), true);
        fs::remove_file(path).unwrap();
        let image = rows.iter().find_map(|r| r.image.as_ref()).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }
    #[test]
    fn html_containers_do_not_hide_markdown_or_leak_tags() {
        let (rows, headings) = render(
            "<div align=\"center\">\n\n# Hello\n\nSome **text**.\n\n</div>\n",
            &Config::default(),
            Path::new("."),
            false,
        );
        assert_eq!(headings[0].text, "Hello");
        let text = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Some text."));
        assert!(!text.contains("<div"));
        let (rows, _) = render(
            "```html\n<div>\n```",
            &Config::default(),
            Path::new("."),
            false,
        );
        assert!(rows.iter().any(|r| r.text.contains("<div>")));
    }
    #[test]
    fn wrapped_rows_keep_heading_mapping() {
        let row = Row {
            text: "▾ 中文 heading".into(),
            heading: Some(0),
            style: Style::default(),
            image: None,
            pixels: None,
            native: None,
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
            Path::new("."),
            false,
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
