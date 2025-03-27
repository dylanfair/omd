#![allow(warnings)]
use std::default;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::encode;
use clap::Parser;
use futures_util::stream::{Stream, StreamExt};
use local_ip_address::local_ip;
use notify::Watcher;
use pulldown_cmark::{html, CowStr, Event, Options, Parser as MdParser};
use tokio::sync::{broadcast, RwLock};
use warp::{sse, Filter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    //The host name
    #[arg(short = 'H', long = "host", default_value = "127.0.0.1")]
    host: String,
    //The port number
    #[arg(short = 'P', long = "port", default_value = "3030")]
    port: u16,
    //Temporary HTML file instead of a server.
    #[arg(short, long)]
    static_mode: bool,
    //Renders the markdown in clipboard
    #[arg(short = 'C', long = "clipboard")]
    clipboard: bool,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    // Check if both file and clipboard flags are provided
    if args.file.is_some() && args.clipboard {
        eprintln!("Error: Cannot use both a file and the clipboard flag at the same time.");
        eprintln!("Please provide either a file or use the --clipboard flag, but not both.");
        std::process::exit(1);
    }

    if args.static_mode {
        run_static_mode(&args)?;
    } else {
        run_server_mode(&args).await?;
    }

    Ok(())
}

fn run_static_mode(args: &Args) -> io::Result<()> {
    let (file_name, markdown_input) = if args.clipboard {
        let mut clipboard = arboard::Clipboard::new().unwrap();
        let content = clipboard.get_text().unwrap_or_else(|err| {
            eprintln!("Error reading from clipboard: {}", err);
            std::process::exit(1);
        });
        (String::from("Clipboard"), content)
    } else {
        match &args.file {
            Some(file_path) => {
                let mut file = File::open(&file_path).unwrap_or_else(|err| {
                    eprintln!("Error opening file {}: {}", file_path.display(), err);
                    std::process::exit(1);
                });
                let mut content = String::new();
                file.read_to_string(&mut content)?;
                (
                    file_path.file_name().unwrap().to_string_lossy().to_string(),
                    content,
                )
            }
            None => {
                let mut content = String::new();
                io::stdin().read_to_string(&mut content)?;
                (String::from("New file"), content)
            }
        }
    };

    let html_output = render_markdown_to_html(&markdown_input);
    let fonts = read_fonts();
    let html_content = build_full_html(&file_name, &html_output, false);

    let temp_file = tempfile::Builder::new()
        .prefix("markdown_preview_")
        .suffix(".html")
        .rand_bytes(5)
        .tempfile()?;
    let temp_path = temp_file.path().to_string_lossy().to_string();

    open_in_browser(temp_path);

    let mut file = temp_file.as_file();
    file.write_all(html_content.as_bytes())?;
    file.flush()?;

    println!("Press Enter to exit...");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(())
}

fn check_for_wsl2() -> bool {
    let wslinterop = PathBuf::from(r"/proc/sys/fs/binfmt_misc/WSLInterop");
    if wslinterop.exists() {
        return true;
    }
    false
}

fn open_in_browser(link: String) {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&link)
            .spawn()
            .expect("Failed to open browser");
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(&["/C", "start", &link])
            .spawn()
            .expect("Failed to open browser");
    }
    #[cfg(target_os = "linux")]
    {
        if check_for_wsl2() {
            std::process::Command::new("powershell.exe")
                .args(&["-c", "start", &link])
                .spawn()
                .expect("Failed to open browser from wsl2 instance");
        } else {
            std::process::Command::new("xdg-open")
                .arg(&link)
                .spawn()
                .expect("Failed to open browser");
        }
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<sse::Event, warp::Error>> + Send>>;

fn event_stream(rx: broadcast::Receiver<()>) -> EventStream {
    let stream = async_stream::stream! {
        let mut rx = rx;
        while let Ok(_) = rx.recv().await {
            yield Ok(sse::Event::default().data("reload"));
        }
    };
    Box::pin(stream)
}

async fn run_server_mode(args: &Args) -> io::Result<()> {
    let (file_path, file_name, markdown_input) = if args.clipboard {
        let mut clipboard = arboard::Clipboard::new().unwrap();
        let content = clipboard.get_text().unwrap_or_else(|err| {
            eprintln!("Error reading from clipboard: {}", err);
            std::process::exit(1);
        });
        (
            PathBuf::from("Clipboard"),
            String::from("Clipboard"),
            content,
        )
    } else {
        let file_path = match &args.file {
            Some(path) => path.clone(),
            None => {
                eprintln!("Error: No input file specified in server mode.");
                std::process::exit(1);
            }
        };
        let file_name = file_path.file_name().unwrap().to_string_lossy().to_string();
        let markdown_input = read_markdown_input(&file_path)?;
        (file_path, file_name, markdown_input)
    };

    let html_output = render_markdown_to_html(&markdown_input);
    let style = read_style_css();
    let fonts = read_fonts();
    let (tx, _) = broadcast::channel::<()>(100);
    let app_state = Arc::new(AppState {
        html_content: Arc::new(RwLock::new(html_output)),
        css_content: style,
        fonts,
        file_path: file_path.clone(),
        notifier: tx.clone(),
        file_name,
    });

    // Start the file watcher task
    let app_state_clone = app_state.clone();
    tokio::task::spawn_blocking(move || watch_markdown_file(app_state_clone));

    // Set up the routes
    let state_filter = warp::any().map(move || app_state.clone());
    let html_route = warp::path::end()
        .and(state_filter.clone())
        .and_then(serve_html);

    let sse_route = warp::path("events")
        .and(warp::get())
        .and(state_filter.clone())
        .and_then(sse_handler);

    let mut host = args.host.clone();
    if args.host == "0.0.0.0" {
        if let Ok(local_ip_address) = local_ip() {
            host = local_ip_address.to_string()
        }
    }

    println!("Server running at http://{}:{}", host, args.port);
    open_in_browser(format!("http://{}:{}", host, args.port));

    let address: IpAddr = args
        .host
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    warp::serve(html_route.or(sse_route))
        .run((address, args.port))
        .await;
    Ok(())
}

fn read_markdown_input(file_path: &PathBuf) -> io::Result<String> {
    let mut file = File::open(&file_path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

fn render_markdown_to_html(markdown_input: &str) -> String {
    let mut options = Options::all();

    let parser = MdParser::new_ext(&markdown_input, options);
    let mut html_output = String::new();
    html::push_html(
        &mut html_output,
        parser.map(|event| match event {
            Event::SoftBreak => Event::Html("<br>".into()),
            Event::InlineMath(s) => {
                let mut str = String::from("<span class=\"math math-inline\">$");
                str.push_str(&s.into_string());
                str.push_str("$</span>");
                Event::Html(CowStr::from(str))
            }
            Event::DisplayMath(s) => {
                let mut str = String::from("<span class=\"math math-display\">$$");
                str.push_str(&s.into_string());
                str.push_str("$$</span>");
                Event::Html(CowStr::from(str))
            }
            _ => event,
        }),
    );
    html_output
}

struct AppState {
    html_content: Arc<RwLock<String>>,
    css_content: String,
    fonts: Fonts,
    file_path: PathBuf,
    notifier: broadcast::Sender<()>,
    file_name: String,
}

fn watch_markdown_file(app_state: Arc<AppState>) {
    if app_state.file_path.to_string_lossy() == "Clipboard" {
        return; // Disable watcher for clipboard input
    }

    use notify::{Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode};
    use std::sync::mpsc::channel;

    enum WatcherType {
        PollWatcher(PollWatcher),
        RecommendedWatcher(RecommendedWatcher),
    }

    let (tx_notify, rx_notify) = channel();
    let watcher = if cfg!(target_os = "linux") {
        let mut watcher = PollWatcher::new(
            tx_notify,
            Config::default().with_poll_interval(Duration::from_millis(500)),
        )
        .unwrap();
        watcher
            .watch(app_state.file_path.as_path(), RecursiveMode::NonRecursive)
            .unwrap();
        WatcherType::PollWatcher(watcher)
    } else {
        let mut watcher = RecommendedWatcher::new(tx_notify, Config::default()).unwrap();
        watcher
            .watch(app_state.file_path.as_path(), RecursiveMode::NonRecursive)
            .unwrap();
        WatcherType::RecommendedWatcher(watcher)
    };

    for res in rx_notify {
        match res {
            Ok(event) => {
                if let EventKind::Modify(_) = event.kind {
                    println!("File changed, updating content...");
                    match std::fs::read_to_string(&app_state.file_path) {
                        Ok(markdown_input) => {
                            let html_output = render_markdown_to_html(&markdown_input);
                            let app_state_clone = app_state.clone();
                            tokio::spawn(async move {
                                let mut html_content = app_state_clone.html_content.write().await;
                                *html_content = html_output;
                                if let Err(e) = app_state_clone.notifier.send(()) {
                                    eprintln!("Error sending notification: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            eprintln!("Error reading file: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("watch error: {:?}", e);
            }
        }
    }
}

async fn sse_handler(app_state: Arc<AppState>) -> Result<impl warp::Reply, warp::Rejection> {
    let rx = app_state.notifier.subscribe();
    let stream = event_stream(rx);
    Ok(warp::sse::reply(warp::sse::keep_alive().stream(stream)))
}

async fn serve_html(app_state: Arc<AppState>) -> Result<impl warp::Reply, warp::Rejection> {
    let html_content = app_state.html_content.read().await;
    let full_html = build_full_html(
        &app_state.file_name,
        &html_content,
        true, // Enable live reload script
    );
    Ok(warp::reply::html(full_html))
}

fn read_style_css() -> String {
    include_str!("../src/static/style.css").to_string()
}

fn read_katex_code() -> KatexCode {
    KatexCode {
        js: include_str!("../src/static/katex/katex.min.js").to_string(),
        css: include_str!("../src/static/katex/katex.min.css").to_string(),
        auto_render: include_str!("../src/static/katex/auto-render.min.js").to_string(),
    }
}

struct KatexCode {
    js: String,
    css: String,
    auto_render: String,
}

struct Fonts {
    font_regular: String,
    font_medium: String,
    font_light: String,
}

fn read_fonts() -> Fonts {
    Fonts {
        font_regular: encode(include_bytes!("./static/fonts/Oswald/Oswald-Regular.ttf")),
        font_medium: encode(include_bytes!("./static/fonts/Oswald/Oswald-Regular.ttf")),
        font_light: encode(include_bytes!("./static/fonts/Oswald/Oswald-Light.ttf")),
    }
}

fn read_favicon() -> String {
    encode(include_bytes!("./static/favicon.ico"))
}

fn build_katex_code() -> String {
    let katex_assets = read_katex_code();

    format!(
        r#"<style>{}</style>
<script>{}</script>
<script>{}</script>"#,
        katex_assets.css, katex_assets.js, katex_assets.auto_render
    )
}

fn build_style() -> String {
    let css = read_style_css();
    let fonts = read_fonts();

    format!(
        r#"
    <style>
        @font-face {{
            font-family: 'Oswald';
            src: url(data:font/truetype;charset=utf-8;base64,{}) format('truetype');
            font-weight: 400;
            font-style: normal;
        }}
        @font-face {{
            font-family: 'Oswald';
            src: url(data:font/truetype;charset=utf-8;base64,{}) format('truetype');
            font-weight: 700;
            font-style: normal;
        }}
        @font-face {{
            font-family: 'Oswald';
            src: url(data:font/truetype;charset=utf-8;base64,{}) format('truetype');
            font-weight: 300;
            font-style: normal;
        }}

        {}

    </style>

    "#,
        fonts.font_regular, fonts.font_medium, fonts.font_light, css,
    )
}

fn build_full_html(file_name: &str, html_output: &str, enable_reload: bool) -> String {
    let reload_script = if enable_reload {
        r#"
        <script>
            var evtSource = new EventSource("/events");
            evtSource.onmessage = function(e) {
                if (e.data === "reload") {
                    location.reload();
                }
            };
        </script>
        "#
    } else {
        ""
    };

    let katex_code_tags = build_katex_code();
    let style = build_style();
    let favicon = read_favicon();

    format!(
        r#"
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <link rel="icon" href="data:image/x-icon;base64,{}">

    {}

    {}

    <script>
        document.addEventListener('DOMContentLoaded', function() {{
            // for rendering the footnotes at the bottom of the page
            const footnotes = document.querySelectorAll('.footnote-definition');
            if (footnotes.length > 0) {{
                const container = document.createElement('div');
                container.id = 'footnote-container';
                footnotes.forEach(footnote => container.appendChild(footnote));
                document.body.appendChild(container);
            }}

            // for katex
            try {{
                renderMathInElement(document.body, {{
                    delimiters: [
                        {{left: '$$', right: '$$', display: true}},
                        {{left: '$', right: '$', display: false}}
                    ],
                    throwOnError : false
                }});
            }} catch (e) {{
                console.error("KaTeX rendering failed:", e);
            }}
        }});
    </script>

    <title>
        {}
    </title>
</head>
<body>
    {}
    {}
</body>
</html>
"#,
        favicon, style, katex_code_tags, file_name, html_output, reload_script
    )
}
